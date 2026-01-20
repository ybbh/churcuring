package tree_sitter_activity_diagram_test

import (
	"testing"

	tree_sitter "github.com/tree-sitter/go-tree-sitter"
	tree_sitter_activity_diagram "github.com/tree-sitter/tree-sitter-activity_diagram/bindings/go"
)

func TestCanLoadGrammar(t *testing.T) {
	language := tree_sitter.NewLanguage(tree_sitter_activity_diagram.Language())
	if language == nil {
		t.Errorf("Error loading ActivityDiagram grammar")
	}
}
