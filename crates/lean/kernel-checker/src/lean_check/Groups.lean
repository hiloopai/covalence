import Lean

open Lean Lean.Elab

/- The wire protocol and CAS live in Rust. This bridge uses the compiler parser,
   elaborator and native snapshot format; it never substitutes source stubs. -/

private def report (j : Json) : IO Unit := do
  let out ← IO.getStdout
  out.putStrLn j.compress
  out.flush

private def reply : IO Json := do
  let line ← (← IO.getStdin).getLine
  IO.ofExcept (Json.parse line)

private def field (j : Json) (key : String) : IO String :=
  IO.ofExcept (j.getObjValAs? String key)

private def checkMessages (messages : MessageLog) : IO Unit := do
  for msg in messages.toList do
    (← IO.getStderr).putStrLn (← msg.toString)
  if messages.hasErrors || messages.toList.any (·.severity == .warning) then
    throw <| IO.userError "group check produced errors or warnings"

private def slice (s : String) (a b : String.Pos.Raw) : String :=
  s.extract (s.pos! a) (s.pos! b)

private def proofRange (stx : Syntax) : Option (String.Pos.Raw × String.Pos.Raw) := do
  guard (stx.getKind == ``Parser.Command.declaration)
  let decl := stx[1]
  guard (decl.getKind == ``Parser.Command.theorem)
  let value := decl[3]
  guard (value.getKind == ``Parser.Command.declValSimple)
  -- Termination clauses and local declarations have effects beyond a proof term.
  guard (value[2].getArgs.all (·.getArgs.isEmpty) && value[3].getArgs.isEmpty)
  return (← value[1].getPos?, ← value[1].getTailPos?)

private def parse (ctx : Parser.InputContext) (s : Command.State)
    (ps : Parser.ModuleParserState) : Syntax × Parser.ModuleParserState × MessageLog :=
  let scope := s.scopes.head!
  Parser.parseCommand ctx
    { env := s.env, options := scope.opts, currNamespace := scope.currNamespace,
      openDecls := scope.openDecls } ps {}

private def elaborate (ctx : Parser.InputContext) (s : Command.State)
    (commands : Array (Syntax × String.Pos.Raw)) : IO Command.State := do
  let mut state : Frontend.State := { commandState := s, parserState := {}, cmdPos := 0 }
  for (cmd, pos) in commands do
    state := { state with cmdPos := pos }
    let (_, next) ← (Frontend.elabCommandAtFrontend cmd).run { inputCtx := ctx } |>.run state
    state := next
    checkMessages state.commandState.messages
    for task in state.commandState.snapshotTasks do
      for snap in task.task.get.getAll do
        checkMessages snap.diagnostics.msgLog
  return { state.commandState with
    messages := {}
    infoState := { enabled := false }
    traceState := {}
    snapshotTasks := #[]
    codeQualityEntryTasks := #[] }

-- Persistent environment maps share unchanged nodes. Pointer equality only
-- skips identical immutable subtrees; changed shapes use the full membership test.
private unsafe def additionsDiff (before : Environment)
    (old new : PersistentHashMap.Node Name ConstantInfo)
    (acc : Array (Name × ConstantInfo)) : Array (Name × ConstantInfo) := Id.run do
  if ptrEq old new then return acc
  let mut acc := acc
  match old, new with
  | .entries os, .entries ns =>
    for i in [:ns.size] do
      match ns[i]! with
      | .null => pure ()
      | .entry n v => if !before.contains n then acc := acc.push (n, v)
      | .ref node =>
        match os[i]?.getD .null with
        | .ref previous => acc := additionsDiff before previous node acc
        | _ => acc := PersistentHashMap.foldlMAux (m := Id) (fun acc n c => if before.contains n then acc else acc.push (n,c)) node acc
  | _, _ =>
    acc := PersistentHashMap.foldlMAux (m := Id)
      (fun acc n c => if before.contains n then acc else acc.push (n,c)) new acc
  return acc

private unsafe def additions (before after : Environment) : Array (Name × ConstantInfo) :=
  additionsDiff before before.constants.map₂.root after.constants.map₂.root #[]

private unsafe def audit (before actual expected : Environment) : IO Unit := do
  let new := additions before actual
  let old := additions before expected
  unless new.size == old.size do
    throw <| IO.userError "replacement changed the set of declarations"
  for (name, info) in new do
    match info, expected.find? name with
    | .thmInfo a, some (.thmInfo b) =>
      unless a.levelParams == b.levelParams && a.type == b.type do
        throw <| IO.userError s!"replacement changed the type of {name}"
      if a.type.hasSorry || a.value.hasSorry then
        throw <| IO.userError s!"replacement contains sorry: {name}"
    | _, _ => throw <| IO.userError s!"replacement adds an unsupported constant: {name}"

private structure Group where
  commands : Array (Syntax × String.Pos.Raw)
  next : Parser.ModuleParserState
  signature : String
  proofs : Bool

private def parseGroup (ctx : Parser.InputContext) (state : Command.State)
    (initial : Parser.ModuleParserState) : IO Group := do
  let mut ps := initial
  let mut commands := #[]
  let mut signature := ""
  let mut proofs := true
  while commands.size < 32 do
    let (cmd, next, msgs) := parse ctx state ps
    checkMessages msgs
    if Parser.isTerminalCommand cmd then break
    let range := proofRange cmd
    if !commands.isEmpty && range.isNone then break
    let text := slice ctx.inputString ps.pos next.pos
    signature := signature ++ match range with
      | some (a, b) => slice ctx.inputString ps.pos a ++ "<proof>" ++ slice ctx.inputString b next.pos
      | none => text
    proofs := proofs && range.isSome
    commands := commands.push (cmd, ps.pos)
    ps := next
    if !proofs then break
  return { commands, next := ps, signature, proofs }

private def reportGroup (index : Nat) (ps : Parser.ModuleParserState) (g : Group) : IO Unit :=
  report <| Json.mkObj [("index", toJson index), ("start", toJson ps.pos.byteIdx),
    ("stop", toJson g.next.pos.byteIdx), ("signature", toJson g.signature),
    ("proofs", toJson g.proofs), ("count", toJson g.commands.size),
    ("leading", toJson ps.hasLeading)]

unsafe def main (args : List String) : IO UInt32 := do
  unless args.length == 4 do
    throw <| IO.userError "expected setup, source, module filename and snapshot directory"
  enableInitializersExecution
  initSearchPath (← findSysroot)
  let setup ← ModuleSetup.load args[0]!
  let source ← IO.FS.readFile args[1]!
  let ctx := Parser.mkInputContext source args[2]!
  let (header, initialParser, messages) ← Parser.parseHeader ctx
  let opts := setup.options.toOptions |>.setBool `warningAsError true |>.setBool `Elab.async false
  let (env, messages) ← processHeaderCore (header.raw.getPos?.getD 0)
    (HeaderSyntax.imports header) (strictOr setup.isModule (HeaderSyntax.isModule header))
    opts messages ctx 0 #[] false setup.name setup.package? setup.importArts
    (headerStx? := header) (origHeaderStx? := header)
  checkMessages messages
  let mut state := Command.mkState env {} opts
  state := { state with infoState := { enabled := false } }
  let mut regions := env.header.regions
  let mut ps := initialParser
  let mut index := 0
  report <| Json.mkObj [("headerEnd", toJson ps.pos.byteIdx),
    ("protocol", toJson "mor/proof-groups/v3")]
  let mode ← field (← reply) "mode"
  let baseline := mode == "baseline"
  unless baseline || mode == "replace" do throw <| IO.userError "invalid operation"
  repeat
    if !baseline then
      let request ← reply
      if (request.getObjValAs? Bool "done").toOption.getD false then break
      index ← IO.ofExcept (request.getObjValAs? Nat "index")
      let start ← IO.ofExcept (request.getObjValAs? Nat "start")
      let leading ← IO.ofExcept (request.getObjValAs? Bool "leading")
      ps := { pos := ⟨start⟩, hasLeading := leading }
      let paths ← IO.ofExcept (request.getObjValAs? (Array String) "loadBefore")
      for path in paths do
        let (saved, region) ← CompactedRegion.read (α := Command.State) path regions
        state := saved
        regions := regions.push region
    let group ← parseGroup ctx state ps
    if group.commands.isEmpty then
      report <| Json.mkObj [("done", toJson true), ("groups", toJson index)]
      break
    reportGroup index ps group
    ps := group.next
    let request ← reply
    let action ← field request "action"
    let before := state
    if baseline || action == "check" then
      state ← elaborate ctx state group.commands
    if baseline then
      let path : System.FilePath := args[3]! / s!"{index}.snap"
      let _ ← CompactedRegion.save path (Name.mkSimple s!"mor-group-{index}") state regions none
        (allowClosures := true)
      let (saved, region) ← CompactedRegion.read (α := Command.State) path regions
      state := saved
      regions := regions.push region
    else
      unless action == "check" || action == "skip" do throw <| IO.userError "invalid group action"
      let path ← field request "snapshot"
      let (saved, region) ← CompactedRegion.read (α := Command.State) path regions
      if action == "check" then audit before.env state.env saved.env
      -- Replacement proofs are checked in isolation. Subsequent groups see the
      -- original context, including its original theorem bodies and extensions.
      state := saved
      regions := regions.push region
    report <| Json.mkObj [("saved", toJson index)]
    index := index + 1
  return 0
