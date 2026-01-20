import XCTest
import SwiftTreeSitter
import TreeSitterScl

final class TreeSitterSclTests: XCTestCase {
    func testCanLoadGrammar() throws {
        let parser = Parser()
        let language = Language(language: tree_sitter_scl())
        XCTAssertNoThrow(try parser.setLanguage(language),
                         "Error loading State Construction Language grammar")
    }
}
