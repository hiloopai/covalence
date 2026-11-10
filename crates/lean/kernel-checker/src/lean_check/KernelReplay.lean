import Lean

open Lean

private def nameParts : Name → List Json
  | .anonymous => []
  | .str p s => nameParts p ++ [Json.arr #[toJson "str", toJson s]]
  | .num p n => nameParts p ++ [Json.arr #[toJson "num", toJson (toString n)]]

private def nameKey (n : Name) : String := (toJson (nameParts n)).compress

private def levelJson : Level → Except String Json
  | .zero => pure <| Json.arr #[toJson "zero"]
  | .succ u => do pure <| Json.arr #[toJson "succ", ← levelJson u]
  | .max u v => do pure <| Json.arr #[toJson "max", ← levelJson u, ← levelJson v]
  | .imax u v => do pure <| Json.arr #[toJson "imax", ← levelJson u, ← levelJson v]
  | .param n => pure <| Json.arr #[toJson "param", toJson (nameKey n)]
  | .mvar _ => throw "unresolved universe metavariable"

private structure Encoding where
  nodes : Array Json := #[]
  seen : ExprStructMap Nat := {}
  canonical : Std.HashMap Json Nat := {}

-- Each term has its own deterministic DAG. Indices never escape that term and
-- do not depend on declaration order. Metadata is erased because the kernel
-- ignores it; these fingerprints cannot describe a metaprogram's observations.
private partial def encodeExpr (e : Expr) : StateT Encoding (Except String) Nat := do
  if let some i := (← get).seen[(⟨e⟩ : ExprStructEq)]? then return i
  if let .mdata _ body := e then return ← encodeExpr body
  let node : Json ← match e with
    | .bvar i => pure <| Json.arr #[toJson "bvar", toJson (toString i)]
    | .fvar _ => throw "free variable in completed declaration"
    | .mvar _ => throw "unresolved expression metavariable"
    | .sort u => do pure <| Json.arr #[toJson "sort", ← levelJson u]
    | .const n us => do pure <| Json.arr #[toJson "const", toJson (nameKey n), toJson (← liftM (us.mapM levelJson))]
    | .app f a => do pure <| Json.arr #[toJson "app", toJson (← encodeExpr f), toJson (← encodeExpr a)]
    | .lam n t b bi => do pure <| Json.arr #[toJson "lam", toJson (nameKey n),
        toJson (← encodeExpr t), toJson (← encodeExpr b), toJson bi.toUInt64.toNat]
    | .forallE n t b bi => do pure <| Json.arr #[toJson "forall", toJson (nameKey n),
        toJson (← encodeExpr t), toJson (← encodeExpr b), toJson bi.toUInt64.toNat]
    | .letE n t v b nondep => do pure <| Json.arr #[toJson "let", toJson (nameKey n),
        toJson (← encodeExpr t), toJson (← encodeExpr v), toJson (← encodeExpr b), toJson nondep]
    | .lit (.natVal n) => pure <| Json.arr #[toJson "nat", toJson (toString n)]
    | .lit (.strVal s) => pure <| Json.arr #[toJson "string", toJson s]
    | .proj n i b => do pure <| Json.arr #[toJson "proj", toJson (nameKey n),
        toJson (toString i), toJson (← encodeExpr b)]
    | .mdata _ _ => throw "unreachable metadata case"
  let index ← match (← get).canonical[node]? with
    | some index => pure index
    | none => do
      let index := (← get).nodes.size
      modify fun s => { s with nodes := s.nodes.push node, canonical := s.canonical.insert node index }
      pure index
  modify fun s => { s with seen := s.seen.insert { val := e } index }
  return index

private def term (e : Expr) : IO Json := do
  let (root, s) ← IO.ofExcept <| (encodeExpr e).run {}
  return Json.mkObj [("nodes", toJson s.nodes), ("root", toJson root)]

private def kind : ConstantInfo → String
  | .thmInfo _ => "theorem"
  | .defnInfo _ => "definition"
  | .opaqueInfo _ => "opaque"
  | .axiomInfo _ => "axiom"
  | .inductInfo _ => "inductive"
  | .ctorInfo _ => "constructor"
  | .recInfo _ => "recursor"
  | .quotInfo _ => "quotient"

private def fallback (ci : ConstantInfo) : Option String := Id.run do
  if ci.isUnsafe || ci.isPartial then return some "unsafe_or_partial"
  match ci with
  | .inductInfo _ | .ctorInfo _ | .recInfo _ => return none
  | .thmInfo _ | .defnInfo _ | .opaqueInfo _ | .axiomInfo _ =>
    return if ci.all != [ci.name] then some "mutual_block" else none
  | _ => return some "unsupported_declaration_kind"

private def structural (ci : ConstantInfo) : IO Json := do
  match ci with
  | .inductInfo v => return Json.mkObj [
      ("params", toJson v.numParams), ("indices", toJson v.numIndices),
      ("ctors", toJson (v.ctors.map nameKey)), ("nested", toJson v.numNested),
      ("recursive", toJson v.isRec), ("reflexive", toJson v.isReflexive)]
  | .ctorInfo v => return Json.mkObj [
      ("induct", toJson (nameKey v.induct)), ("index", toJson v.cidx),
      ("params", toJson v.numParams), ("fields", toJson v.numFields)]
  | .recInfo v =>
    let rules ← v.rules.mapM fun r => do
      pure <| Json.mkObj [("ctor", toJson (nameKey r.ctor)),
        ("fields", toJson r.nfields), ("rhs", ← term r.rhs)]
    return Json.mkObj [("params", toJson v.numParams), ("indices", toJson v.numIndices),
      ("motives", toJson v.numMotives), ("minors", toJson v.numMinors),
      ("rules", toJson rules), ("k", toJson v.k)]
  | _ => return Json.null

/--
Whether a kernel check that uses this theorem depends on its type alone.

A theorem's value is a proof of a proposition, and the kernel identifies any
two proofs of the same proposition. It only inspects a proof when reducing an
eliminator into data whose major premise is that proof. That needs a
large-eliminating proposition: one whose recursor targets any universe. Such a
reduction still ignores the proof when the proposition is K-like (`Eq`,
`HEq`, `True`), since the kernel rebuilds the constructor from the type, or
has no constructors. Anything else, or any conclusion the kernel cannot reduce
to an inductive proposition, counts as depending on the proof.
-/
private partial def statementOnly (env : Environment) (locals : Std.HashMap Name ConstantInfo)
    (ci : ConstantInfo) : Bool :=
  match ci with
  | .thmInfo info => go {} info.type 0
  | _ => false
where
  find (name : Name) : Option ConstantInfo := locals[name]? <|> env.find? name
  propositional : Expr → Bool
    | .forallE _ _ b _ => propositional b
    | .sort .zero => true
    | _ => false
  go (lctx : LocalContext) (e : Expr) (depth : Nat) : Bool :=
    match Kernel.whnf env lctx e with
    | .error _ => false
    | .ok (.forallE n d b bi) =>
      let fvarId : FVarId := ⟨Name.mkNum `_mor_statement depth⟩
      go (lctx.mkLocalDecl fvarId n d bi) (b.instantiate1 (.fvar fvarId)) (depth + 1)
    | .ok conclusion =>
      match conclusion.getAppFn with
      | .const name _ =>
        match find name, find (mkRecName name) with
        | some (.inductInfo v), some (.recInfo r) =>
          propositional v.type &&
            (r.levelParams.length == v.levelParams.length || r.k || v.ctors.isEmpty)
        | _, _ => false
      | _ => false

private def interface (ci : ConstantInfo) : IO Json := do
  let hints := match ci.hints with
    | .opaque => Json.arr #[toJson "opaque"]
    | .abbrev => Json.arr #[toJson "abbrev"]
    | .regular h => Json.arr #[toJson "regular", toJson h.toNat]
  return Json.mkObj [("kind", toJson (kind ci)), ("name", toJson (nameKey ci.name)),
    ("levels", toJson (ci.levelParams.map nameKey)), ("type", ← term ci.type),
    ("hints", hints), ("unsafe", toJson ci.isUnsafe), ("partial", toJson ci.isPartial),
    ("all", toJson (ci.all.map nameKey)), ("structural", ← structural ci)]

private def emit (json : Json) : IO Unit := do
  (← IO.getStdout).putStrLn json.compress
  (← IO.getStdout).flush

private def reply : IO Json := do
  IO.ofExcept <| Json.parse (← (← IO.getStdin).getLine)

-- These are Lake's integrity checks, not the cryptographic cache identity.
private def lakeHash (bytes : ByteArray) : String :=
  let digits := Nat.toDigits 16 (mixHash 1723 (hash bytes)).toNat
  String.ofList (List.replicate (16 - digits.length) '0' ++ digits)

private def artifacts (file : System.FilePath) : IO (Array System.FilePath) := do
  let trace ← IO.ofExcept <| Json.parse (← IO.FS.readFile (file.withExtension "trace"))
  let outputs := trace.getObjValD "outputs" |>.getObjValD "o"
  let outputs ← match outputs with
    | .str one => pure #[one]
    | .arr values => values.mapM fun v => IO.ofExcept v.getStr?
    | _ => throw <| IO.userError "missing artifact outputs in Lake trace"
  let mut result := #[]
  for suffix in ["olean", "olean.server", "olean.private"] do
    let path := file.withExtension suffix
    let expected := outputs.find? (·.endsWith ("." ++ suffix))
    if let some expected := expected then
      let bytes ← IO.FS.readBinFile path
      unless expected == lakeHash bytes ++ "." ++ suffix do
        throw <| IO.userError s!"stale or corrupted artifact: {path}"
      result := result.push path
    else if ← path.pathExists then
      throw <| IO.userError s!"artifact absent from Lake trace: {path}"
  unless !result.isEmpty && result[0]! == file do
    throw <| IO.userError "missing primary olean artifact"
  return result

unsafe def main (args : List String) : IO UInt32 := do
  let (moduleName, importArts, artifactPath, fresh) ← match args with
    | ["--search-path", moduleName, artifactPath] =>
      pure (moduleName.toName, {}, artifactPath, false)
    | [setupPath, artifactPath] => do
      let setup ← ModuleSetup.load setupPath
      pure (setup.name, setup.importArts, artifactPath, false)
    | [setupPath, artifactPath, "--fresh"] => do
      let setup ← ModuleSetup.load setupPath
      pure (setup.name, setup.importArts, artifactPath, true)
    | _ => throw <| IO.userError "expected prepared setup or --search-path module, compiled artifact, optional --fresh"
  initSearchPath (← findSysroot)
  let started ← IO.monoMsNow
  let parts ← readModuleDataParts (← artifacts artifactPath)
  let some (mod, _) := parts[0]? | throw <| IO.userError "missing module parts"
  let some (last, _) := parts[parts.size - 1]? | throw <| IO.userError "missing last module part"
  if mod.isModule && parts.size != 3 then
    throw <| IO.userError "module-system replay requires exported, server and private parts"
  let level := if mod.isModule then OLeanLevel.exported else OLeanLevel.private
  let (_, state) ← importModulesCore mod.imports level importArts |>.run
  let env ← finalizeImport state mod.imports {} 0 false false
    (level := level) (isModule := mod.isModule)
  let mut constants : Std.HashMap Name ConstantInfo := {}
  if last.constNames.size != last.constants.size then
    throw <| IO.userError "inconsistent module declaration arrays"
  for name in last.constNames, ci in last.constants do
    if name != ci.name || constants.contains name || env.contains name then
      throw <| IO.userError s!"duplicate or inconsistent declaration identity: {name}"
    if ci.type.hasSorry || (ci.value? true).any (·.hasSorry) then
      throw <| IO.userError s!"declaration contains sorry: {name}"
    constants := constants.insert name ci
  let loaded ← IO.monoMsNow
  -- Benchmark control: the same artifact loader and upstream full replay,
  -- without term encoding, CAS lookup, or unchecked admission.
  if fresh then
    discard <| env.toKernelEnv.replay constants
    emit <| Json.mkObj [("done", toJson true), ("load_ms", toJson (loaded - started)),
      ("replay_ms", toJson ((← IO.monoMsNow) - loaded))]
    return 0
  let mut inputs := #[]
  for module in env.header.moduleNames do
    let files ← match importArts.find? module with
      | some arts => pure (arts.oleanParts (inServer := level ≥ .server) ++ arts.irParts)
      | none => do
        let file ← findOLean module
        let mut files := #[file]
        for suffix in ["olean.server", "olean.private", "ir.sig", "ir"] do
          let part := file.withExtension suffix
          if ← part.pathExists then files := files.push part
        pure files
    for i in [:files.size] do
      inputs := inputs.push <| Json.mkObj [("module", toJson (nameKey module)),
        ("part", toJson i), ("path", toJson files[i]!)]
  emit <| Json.mkObj [("protocol", toJson "mor/kernel-replay/v1"),
    ("module", toJson (nameKey moduleName)), ("imports", toJson mod.imports),
    ("is_module", toJson mod.isModule), ("declarations", toJson constants.size),
    ("load_ms", toJson (loaded - started)), ("inputs", toJson inputs)]
  let sorted := constants.toArray.qsort fun a b => nameKey a.1 < nameKey b.1
  for (name, ci) in sorted do
    let reason := fallback ci
    let signature ← if reason.isNone then interface ci else pure Json.null
    let body ← if reason.isNone then
      ci.value? true |>.mapM term
      else pure none
    let deps := match ci with
      | .recInfo v => v.rules.foldl (fun (deps : NameSet) (rule : RecursorRule) =>
          (deps ++ rule.rhs.getUsedConstantsAsSet).insert rule.ctor) ci.getUsedConstantsAsSet
      | .ctorInfo v => ci.getUsedConstantsAsSet.insert v.induct
      | _ => ci.getUsedConstantsAsSet
    let deps := deps.toList.filter (constants.contains ·)
    let typeDeps := ci.type.getUsedConstantsAsSet.toList.filter (constants.contains ·)
    emit <| Json.mkObj [("name", toJson name.toString), ("name_key", toJson (nameKey name)),
      ("kind", toJson (kind ci)), ("fallback", toJson reason),
      ("interface", signature), ("body", body.getD Json.null),
      ("dependencies", toJson (deps.map nameKey)),
      ("statement_only", toJson (reason.isNone && statementOnly env constants ci)),
      ("interface_dependencies", toJson (typeDeps.map nameKey))]
  emit <| Json.mkObj [("encoded", toJson true),
    ("encoding_ms", toJson ((← IO.monoMsNow) - loaded))]
  let accepted ← IO.ofExcept <| (← reply).getObjValAs? (Array String) "reuse"
  let names : Std.HashMap String Name := sorted.foldl
    (fun m (name, _) => m.insert (nameKey name) name) {}
  let mut admitted : NameSet := {}
  let mut checkedEnv := env.toKernelEnv
  let checking ← IO.monoMsNow
  for key in accepted do
    let some name := names[key]? | throw <| IO.userError "unknown reuse identity"
    let some (.thmInfo info) := constants[name]? |
      throw <| IO.userError "reuse is restricted to distinct theorem declarations"
    if admitted.contains name then
      throw <| IO.userError "reuse is restricted to distinct theorem declarations"
    -- Only the trusted Rust parent authorizes this path, after matching a
    -- successful receipt for the full proof and its complete pinned context.
    -- Misses still go through upstream replay, including inductive metadata.
    checkedEnv ← match checkedEnv.addDeclWithoutChecking (.thmDecl info) with
      | .ok next => pure next
      | .error err => throw <| IO.userError (← err.toMessageData {} |>.toString)
    admitted := admitted.insert name
  -- Admitted theorems stay in the replayed set on purpose. Upstream replay
  -- still visits each one in dependency order and first replays every constant
  -- its type and proof use, so a later consumer finds them all. It then skips
  -- the theorem's own kernel check because an identical theorem is already in
  -- the environment (its duplicate-theorem case). Outside the set, those
  -- constants would arrive only at their own turn, after some consumers.
  discard <| checkedEnv.replay constants
  emit <| Json.mkObj [("done", toJson true), ("reused", toJson accepted.size),
    ("replay_ms", toJson ((← IO.monoMsNow) - checking))]
  return 0
