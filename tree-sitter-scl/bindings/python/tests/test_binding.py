import tree_sitter_scl
from unittest import TestCase

from tree_sitter import Language, Parser


class TestLanguage(TestCase):
    def test_can_load_grammar(self):
        try:
            Parser(Language(tree_sitter_scl.language()))
        except Exception:
            self.fail("Error loading State Construction Language grammar")
