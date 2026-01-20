import XCTest
import SwiftTreeSitter
import TreeSitterActivityDiagram

final class TreeSitterActivityDiagramTests: XCTestCase {
    func testCanLoadGrammar() throws {
        let parser = Parser()
        let language = Language(language: tree_sitter_activity_diagram())
        XCTAssertNoThrow(try parser.setLanguage(language),
                         "Error loading ActivityDiagram grammar")
    }
}
