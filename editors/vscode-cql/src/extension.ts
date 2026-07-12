// CQL VSCode extension: tree-sitter (wasm) powered syntax highlighting.
//
// Architecture: load the tree-sitter-cql wasm grammar (built by
// `tree-sitter build --wasm` from ../../crates/tree-sitter-cql and copied
// into the extension root) plus queries/highlights.scm, and expose the
// captures through a DocumentSemanticTokensProvider.
//
// If tree-sitter-cql.wasm is missing the extension degrades gracefully: a
// note is written to the "CQL" output channel once and no provider is
// registered (see README.md for how to build the wasm grammar).

import * as vscode from 'vscode';
import * as fs from 'fs';
import * as path from 'path';
import Parser from 'web-tree-sitter';

// ------------------------------------------------------------------
// Capture name -> semantic token mapping
// ------------------------------------------------------------------

const TOKEN_TYPES = [
    'keyword', 'type', 'typeParameter', 'function', 'variable',
    'parameter', 'property', 'enumMember', 'namespace',
    'string', 'number', 'comment', 'operator',
];
const TOKEN_MODIFIERS = ['declaration', 'readonly', 'builtin', 'escape'];

interface TokenSpec {
    type: string;
    modifiers?: string[];
}

const CAPTURE_MAP: Record<string, TokenSpec | null> = {
    'keyword': { type: 'keyword' },
    'type': { type: 'type' },
    'type.builtin': { type: 'type', modifiers: ['builtin'] },
    'type.parameter': { type: 'typeParameter' },
    'namespace': { type: 'namespace' },
    'function': { type: 'function' },
    'variable': { type: 'variable' },
    'variable.parameter': { type: 'parameter' },
    'variable.builtin': { type: 'variable', modifiers: ['builtin'] },
    'constant': { type: 'variable', modifiers: ['readonly'] },
    'enumMember': { type: 'enumMember' },
    'property': { type: 'property' },
    'string': { type: 'string' },
    'string.escape': { type: 'string', modifiers: ['escape'] },
    'number': { type: 'number' },
    'comment': { type: 'comment' },
    'operator': { type: 'operator' },
    // Punctuation has no standard semantic token type; TextMate/defaults
    // handle it, so these captures are intentionally dropped.
    'punctuation.bracket': null,
    'punctuation.delimiter': null,
    'punctuation.special': null,
};

function mapCapture(name: string): TokenSpec | null {
    const exact = CAPTURE_MAP[name];
    if (exact !== undefined) {
        return exact;
    }
    // Fallback: strip the sub-scope (e.g. `foo.bar` -> `foo`).
    const dot = name.indexOf('.');
    if (dot > 0) {
        const base = CAPTURE_MAP[name.slice(0, dot)];
        if (base) {
            return base;
        }
    }
    return null;
}

// ------------------------------------------------------------------
// Parser state
// ------------------------------------------------------------------

let output: vscode.OutputChannel;
let parser: Parser | null = null;
let highlightQuery: Parser.Query | null = null;

interface DocState {
    text: string;
    tree: Parser.Tree;
}

const docs = new Map<string, DocState>();

function byteLength(s: string): number {
    return Buffer.byteLength(s, 'utf8');
}

/** tree-sitter Point (row, byte column) for a UTF-16 offset into `text`. */
function pointAt(text: string, offset: number): Parser.Point {
    const before = text.slice(0, offset);
    const row = (before.match(/\n/g) ?? []).length;
    const lineStart = before.lastIndexOf('\n') + 1;
    return { row, column: byteLength(before.slice(lineStart)) };
}

/** UTF-16 character index of `byteCol` bytes into a single line of text. */
function byteColToChar(line: string, byteCol: number): number {
    return Buffer.from(line, 'utf8').subarray(0, byteCol).toString('utf8').length;
}

function getState(document: vscode.TextDocument): DocState | null {
    if (!parser) {
        return null;
    }
    const key = document.uri.toString();
    const text = document.getText();
    let state = docs.get(key);
    if (!state || state.text !== text) {
        // First sight of the document, or we fell out of sync (e.g. the
        // document was changed while the extension was inactive): reparse
        // from scratch.
        state?.tree.delete();
        state = { text, tree: parser.parse(text) };
        docs.set(key, state);
    }
    return state;
}

// ------------------------------------------------------------------
// Initialization
// ------------------------------------------------------------------

async function initParser(context: vscode.ExtensionContext): Promise<boolean> {
    const grammarWasm = context.asAbsolutePath('tree-sitter-cql.wasm');
    if (!fs.existsSync(grammarWasm)) {
        output.appendLine(
            'tree-sitter-cql.wasm not found in the extension root; ' +
            'tree-sitter semantic highlighting is disabled. ' +
            'See README.md ("Building the wasm grammar") for build steps.'
        );
        return false;
    }
    try {
        await Parser.init({
            locateFile(scriptName: string, _scriptDirectory: string) {
                // web-tree-sitter's own runtime wasm ships inside the
                // npm package and is bundled with the extension.
                if (scriptName === 'tree-sitter.wasm') {
                    return path.join(
                        context.extensionPath,
                        'node_modules', 'web-tree-sitter', 'tree-sitter.wasm'
                    );
                }
                return scriptName;
            },
        });
        const language = await Parser.Language.load(grammarWasm);
        parser = new Parser();
        parser.setLanguage(language);
        const querySource = await fs.promises.readFile(
            context.asAbsolutePath(path.join('queries', 'highlights.scm')),
            'utf8'
        );
        highlightQuery = language.query(querySource);
        output.appendLine('tree-sitter CQL grammar loaded; semantic highlighting active.');
        return true;
    } catch (err) {
        output.appendLine(`Failed to initialize tree-sitter highlighting: ${err}`);
        parser = null;
        highlightQuery = null;
        return false;
    }
}

// ------------------------------------------------------------------
// Semantic tokens provider
// ------------------------------------------------------------------

const legend = new vscode.SemanticTokensLegend(TOKEN_TYPES, TOKEN_MODIFIERS);

const provider: vscode.DocumentSemanticTokensProvider = {
    provideDocumentSemanticTokens(document, _token) {
        const state = getState(document);
        if (!state || !highlightQuery) {
            return new vscode.SemanticTokensBuilder(legend).build();
        }
        const builder = new vscode.SemanticTokensBuilder(legend);
        // Deduplicate: several patterns can capture the same range (e.g. a
        // scoped keyword capture and the global fallback). Patterns are
        // ordered specific-first in highlights.scm, so the first capture
        // of a range wins.
        const seen = new Set<string>();
        for (const capture of highlightQuery.captures(state.tree.rootNode)) {
            const spec = mapCapture(capture.name);
            if (!spec) {
                continue;
            }
            const node = capture.node;
            const s = node.startPosition;
            const e = node.endPosition;
            const key = `${s.row}:${s.column}-${e.row}:${e.column}`;
            if (seen.has(key)) {
                continue;
            }
            seen.add(key);
            pushToken(builder, document, s, e, spec);
        }
        return builder.build();
    },
};

function pushToken(
    builder: vscode.SemanticTokensBuilder,
    document: vscode.TextDocument,
    start: Parser.Point,
    end: Parser.Point,
    spec: TokenSpec
): void {
    const lastRow = document.lineCount - 1;
    const firstRow = Math.min(start.row, lastRow);
    const endRow = Math.min(end.row, lastRow);
    for (let row = firstRow; row <= endRow; row++) {
        const lineText = document.lineAt(row).text;
        const startByte = row === start.row ? start.column : 0;
        const endByte = row === end.row ? end.column : byteLength(lineText);
        if (endByte <= startByte) {
            continue;
        }
        const startChar = byteColToChar(lineText, startByte);
        const endChar = byteColToChar(lineText, endByte);
        if (endChar <= startChar) {
            continue;
        }
        builder.push(
            new vscode.Range(row, startChar, row, endChar),
            spec.type,
            spec.modifiers ?? []
        );
    }
}

// ------------------------------------------------------------------
// Incremental reparsing
// ------------------------------------------------------------------

function onDidChangeTextDocument(event: vscode.TextDocumentChangeEvent): void {
    if (event.document.languageId !== 'cql' || !parser) {
        return;
    }
    const key = event.document.uri.toString();
    const state = docs.get(key);
    if (!state) {
        // Never parsed yet; the provider will do a full parse on demand.
        return;
    }
    let text = state.text;
    // contentChanges are sorted by range in reverse document order, so each
    // rangeOffset still refers to a valid position in the intermediate text
    // after the previous (later-in-document) changes have been applied.
    for (const change of event.contentChanges) {
        const startIndex = byteLength(text.slice(0, change.rangeOffset));
        const oldEndIndex = startIndex +
            byteLength(text.substr(change.rangeOffset, change.rangeLength));
        const newEndIndex = startIndex + byteLength(change.text);
        const startPosition = pointAt(text, change.rangeOffset);
        const oldEndPosition = pointAt(text, change.rangeOffset + change.rangeLength);
        text = text.slice(0, change.rangeOffset) +
            change.text +
            text.slice(change.rangeOffset + change.rangeLength);
        const newEndPosition = pointAt(text, change.rangeOffset + change.text.length);
        state.tree.edit({
            startIndex,
            oldEndIndex,
            newEndIndex,
            startPosition,
            oldEndPosition,
            newEndPosition,
        });
    }
    state.text = text;
    const oldTree = state.tree;
    state.tree = parser.parse(text, oldTree);
    oldTree.delete();
}

// ------------------------------------------------------------------
// Activation
// ------------------------------------------------------------------

export async function activate(context: vscode.ExtensionContext): Promise<void> {
    output = vscode.window.createOutputChannel('CQL');
    context.subscriptions.push(output);

    if (!(await initParser(context))) {
        return;
    }

    const selector: vscode.DocumentSelector = [{ language: 'cql' }];
    context.subscriptions.push(
        vscode.languages.registerDocumentSemanticTokensProvider(selector, provider, legend),
        vscode.workspace.onDidChangeTextDocument(onDidChangeTextDocument),
        vscode.workspace.onDidCloseTextDocument((document) => {
            const key = document.uri.toString();
            const state = docs.get(key);
            if (state) {
                state.tree.delete();
                docs.delete(key);
            }
        })
    );
}

export function deactivate(): void {
    for (const state of docs.values()) {
        state.tree.delete();
    }
    docs.clear();
    highlightQuery?.delete();
    parser?.delete();
    parser = null;
    highlightQuery = null;
}
