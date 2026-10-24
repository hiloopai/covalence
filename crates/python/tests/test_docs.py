"""The public API is documented, under its public names, with examples that run."""

import ast
import doctest
import importlib.resources
import inspect
from pathlib import Path
import types
import unittest

import covalence


def public_classes():
    """The classes `covalence.__all__` names."""
    for name in covalence.__all__:
        value = getattr(covalence, name)
        if isinstance(value, type):
            yield name, value


def public_members(cls):
    """The members `cls` defines: public names, and dunder methods with docs of their own."""
    for name, value in vars(cls).items():
        if not name.startswith("_"):
            yield name, getattr(cls, name)
        elif isinstance(value, (types.FunctionType, types.MethodDescriptorType)):
            yield name, value


def public_docs():
    """Each public class and member by qualified name, with its docstring."""
    for name, cls in public_classes():
        yield name, cls.__doc__
        for member, value in public_members(cls):
            yield f"{name}.{member}", value.__doc__
    for name in covalence.config.__all__:
        yield f"config.{name}", getattr(covalence.config, name).__doc__


class DocsTests(unittest.TestCase):
    def test_public_names_are_covalence(self):
        self.assertNotIn("_native", covalence.__all__)
        for name, cls in public_classes():
            with self.subTest(name=name):
                self.assertEqual((cls.__module__, cls.__qualname__), ("covalence", name))
                for member, value in public_members(cls):
                    qualname = getattr(value, "__qualname__", None)
                    if qualname is not None:
                        self.assertEqual(qualname, f"{name}.{member}")

    def test_public_api_is_documented(self):
        self.assertTrue(covalence.__doc__)
        documented = 0
        for name, doc in public_docs():
            with self.subTest(name=name):
                self.assertTrue(doc and doc.strip(), "no docstring")
                documented += 1
        self.assertGreater(documented, 60)

    def test_stub_docstrings_match_runtime(self):
        stub = importlib.resources.files("covalence").joinpath("_native.pyi").read_text(encoding="utf-8")
        classes = {node.name: node for node in ast.parse(stub).body if isinstance(node, ast.ClassDef)}
        for name, cls in public_classes():
            if hasattr(covalence._native, name):
                with self.subTest(name=name):
                    self.assertIn(name, classes)
                    defined = {item.name for item in classes[name].body if isinstance(item, ast.FunctionDef)}
                    self.assertLessEqual({member for member, _ in public_members(cls)}, defined)
        compared = 0
        for node in classes.values():
            cls = getattr(covalence._native, node.name)
            pairs = [(node.name, node, cls)]
            for item in node.body:
                if isinstance(item, ast.FunctionDef):
                    pairs.append((f"{node.name}.{item.name}", item, inspect.getattr_static(cls, item.name)))
            for name, item, value in pairs:
                if isinstance(value, types.WrapperDescriptorType) or item.name in ("__init__", "__new__"):
                    continue  # Python's own slots, whose docs PyO3 cannot set.
                if isinstance(value, staticmethod):
                    value = value.__func__
                with self.subTest(name=name):
                    self.assertEqual(ast.get_docstring(item), inspect.cleandoc(value.__doc__ or "") or None)
                    compared += 1
        self.assertGreater(compared, 60)

    def test_config_stub_docstrings_match_runtime(self):
        stub = importlib.resources.files("covalence").joinpath("config.pyi").read_text(encoding="utf-8")
        functions = {node.name: node for node in ast.parse(stub).body if isinstance(node, ast.FunctionDef)}
        self.assertEqual(set(functions), set(covalence.config.__all__))
        for name, node in functions.items():
            with self.subTest(name=name):
                self.assertEqual(
                    ast.get_docstring(node),
                    inspect.cleandoc(getattr(covalence.config, name).__doc__ or ""),
                )

    def test_examples_are_found(self):
        found = {case._dt_test.name for case in doctests()}
        expected = {f"covalence.{name}" for name, doc in public_docs() if ">>>" in (doc or "")}
        self.assertEqual(found, expected | {"README.md"})


class NativeFinder(doctest.DocTestFinder):
    """`DocTestFinder`, plus the examples in native static methods and properties.

    `DocTestFinder` skips both: a PyO3 static method has no `__module__`, so
    it looks foreign, and a PyO3 property is a getset descriptor, which it does
    not visit. It finds native methods and Python members itself.
    """

    def find(self, obj, name=None, module=None, globs=None, extraglobs=None):
        tests = super().find(obj, name, module, globs, extraglobs)
        name = name or obj.__name__
        globs = vars(obj) if globs is None else globs
        for cls_name, cls in vars(obj).items():
            if not isinstance(cls, type) or cls.__module__ != obj.__name__:
                continue
            for member, value in vars(cls).items():
                if isinstance(value, staticmethod) and inspect.isbuiltin(value.__func__):
                    value = value.__func__
                elif not isinstance(value, types.GetSetDescriptorType):
                    continue
                qualname = f"{name}.{cls_name}.{member}"
                tests += super().find(value, qualname, False, globs, extraglobs)
        return tests


def doctests():
    """The examples in `covalence`'s docstrings and in the README."""
    readme = Path(__file__).resolve().parent.parent / "README.md"
    options = doctest.ELLIPSIS
    suite = doctest.DocTestSuite(covalence, test_finder=NativeFinder(), optionflags=options)
    suite.addTests(doctest.DocTestSuite(covalence.config, optionflags=options))
    suite.addTests(doctest.DocFileSuite(str(readme), module_relative=False, optionflags=options))
    return suite


def load_tests(loader, tests, pattern):
    tests.addTests(doctests())
    return tests


if __name__ == "__main__":
    unittest.main()
