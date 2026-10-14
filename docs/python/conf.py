"""Sphinx configuration for the covalence Python API docs."""

import covalence

project = "covalence"
release = covalence.__version__
version = release

extensions = ["sphinx.ext.autodoc", "sphinx.ext.intersphinx"]

# Docstrings mark code with single backticks.
default_role = "literal"

autodoc_default_options = {"members": True}

intersphinx_mapping = {"python": ("https://docs.python.org/3", None)}

html_theme = "furo"
html_title = f"covalence {release}"


def _hide_blobfact_signature(app, what, name, obj, options, signature, ret):
    # BlobFact inherits Blob's constructor signature, but cannot be constructed
    # directly; use BlobFact.of instead.
    if what == "class" and name == "covalence.BlobFact":
        return "", None
    return None


def setup(app):
    app.connect("autodoc-process-signature", _hide_blobfact_signature)
