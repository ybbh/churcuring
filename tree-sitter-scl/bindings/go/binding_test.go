package tree_sitter_scl_test

import (
	"testing"

	tree_sitter "github.com/tree-sitter/go-tree-sitter"
	tree_sitter_scl "github.com/tree-sitter/tree-sitter-scl/bindings/go"
)

func TestCanLoadGrammar(t *testing.T) {
	language := tree_sitter.NewLanguage(tree_sitter_scl.Language())
	if language == nil {
		t.Errorf("Error loading State Construction Language grammar")
	}
}
