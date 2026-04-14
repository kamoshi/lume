/**
 * CodeMirror 6 extensions for the Lume language backed by lume-wasm.
 *
 * Usage:
 *   import { lumeExtensions } from './lume-wasm/codemirror'
 *   import { EditorView, basicSetup } from 'codemirror'
 *
 *   new EditorView({ extensions: [basicSetup, ...lumeExtensions()], parent })
 *
 * The WASM module is initialised lazily on first use — no manual `await init()`
 * needed. Peer deps: @codemirror/lint, @codemirror/view, @codemirror/state.
 */

import type { Diagnostic } from "@codemirror/lint";
import { linter } from "@codemirror/lint";
import type { Extension } from "@codemirror/state";
import { hoverTooltip } from "@codemirror/view";

// Import from the wasm-pack output. Adjust the specifier to match your bundler
// setup: use 'lume-wasm' if installed as an npm package, or a relative path
// like './pkg/lume_wasm.js' for a local build.
import init, { lint as wasmLint, type_at as wasmTypeAt } from "./pkg";

// Single shared init promise — the WASM binary loads once.
const ready: Promise<void> = init().then(() => {});

// ── Public API ────────────────────────────────────────────────────────────────

/** Returns the full set of Lume CodeMirror extensions. Drop into `extensions`. */
export function lumeExtensions(): Extension[] {
	return [lumeLinter(), lumeHover()];
}

// ── Linter (diagnostics) ──────────────────────────────────────────────────────

function lumeLinter() {
	return linter(async (view): Promise<readonly Diagnostic[]> => {
		await ready;
		const src = view.state.doc.toString();

		let raw: { from: number; to: number; message: string }[];
		try {
			raw = JSON.parse(wasmLint(src));
		} catch {
			return [];
		}

		return raw.map((d) => ({
			from: d.from,
			to: Math.max(d.to, d.from + 1), // CM requires from < to
			severity: "error" as const,
			message: d.message,
		}));
	});
}

// ── Hover tooltip (type-at-cursor) ────────────────────────────────────────────

function lumeHover() {
	return hoverTooltip(async (view, pos) => {
		await ready;
		const src = view.state.doc.toString();
		const ty = wasmTypeAt(src, pos);
		if (!ty) return null;

		return {
			pos,
			create() {
				const dom = document.createElement("div");
				dom.className = "cm-lume-type";
				dom.textContent = ty;
				return { dom };
			},
		};
	});
}
