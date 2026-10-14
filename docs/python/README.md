# Python API docs

Sphinx builds the `covalence` docstrings into HTML, here and on Read the Docs.

- `.readthedocs.yaml` at the repository root configures Read the Docs.
- Pages document the public `covalence` package, never `covalence._native`.
- `crates/python/tests/test_docs.py` runs the doctests, so Sphinx does not.

Build them locally from the repository root, with Rust and
[uv](https://docs.astral.sh/uv/) installed:

```sh
uv venv -p 3.13 target/docs/venv
uv pip install -p target/docs/venv ./crates/python -r docs/python/requirements.txt
target/docs/venv/bin/sphinx-build -W -b html docs/python target/docs/html
```
