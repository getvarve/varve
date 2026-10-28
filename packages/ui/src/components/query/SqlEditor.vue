<script setup lang="ts">
import { MariaSQL, MySQL, PostgreSQL, sql } from '@codemirror/lang-sql';
import { HighlightStyle, syntaxHighlighting } from '@codemirror/language';
import { EditorSelection, EditorState, Prec, StateEffect, StateField } from '@codemirror/state';
import type { Extension } from '@codemirror/state';
import { Decoration, EditorView, keymap } from '@codemirror/view';
import type { DecorationSet } from '@codemirror/view';
import { tags } from '@lezer/highlight';
import { basicSetup } from 'codemirror';
import { onBeforeUnmount, onMounted, ref, watch } from 'vue';
import type { Engine } from '../../api/records';
import { markedErrorRuns, rememberMarkedRun } from '../../lib/errorPosition';
import { commandOfLine } from '../../lib/redisHelp';
import { redisLanguage } from '../../lib/redisMode';

/**
 * The query editor: CodeMirror 6 with the SQL dialect of the engine (plain
 * Redis commands, one per line, for Redis). Cmd/Ctrl+Enter runs the
 * selection, or the whole text when nothing is selected.
 */
const props = defineProps<{
  modelValue: string;
  engine: Engine;
  /** Bumped by the parent when `modelValue` was replaced from outside. */
  revision: number;
  /** Range to mark as the error position, in editor offsets; marked once per `key` (run). */
  errorRange: { key: number; from: number; to: number; doc: string } | null;
  label: string;
}>();
const emit = defineEmits<{ 'update:modelValue': [value: string]; run: [text: string, offset: number]; help: [command: string | null] }>();

const host = ref<HTMLElement | null>(null);
let view: EditorView | null = null;

const setError = StateEffect.define<{ from: number; to: number } | null>();
const errorField = StateField.define<DecorationSet>({
  create: () => Decoration.none,
  update(marks, tr) {
    let next = marks.map(tr.changes);
    for (const effect of tr.effects) {
      if (effect.is(setError)) {
        next = effect.value && effect.value.to > effect.value.from
          ? Decoration.set([Decoration.mark({ class: 'cm-error-mark' }).range(effect.value.from, effect.value.to)])
          : Decoration.none;
      }
    }
    if (tr.docChanged) next = Decoration.none;
    return next;
  },
  provide: (field) => EditorView.decorations.from(field),
});

const highlight = HighlightStyle.define([
  { tag: [tags.keyword, tags.operatorKeyword, tags.modifier], color: 'var(--code-keyword)' },
  { tag: [tags.string, tags.special(tags.string)], color: 'var(--code-string)' },
  { tag: [tags.number, tags.bool, tags.null], color: 'var(--code-number)' },
  { tag: [tags.comment, tags.lineComment, tags.blockComment], color: 'var(--code-comment)', fontStyle: 'italic' },
  { tag: [tags.typeName, tags.standard(tags.name)], color: 'var(--code-keyword)' },
]);

const theme = EditorView.theme({
  '&': { height: '100%', backgroundColor: 'var(--surface)', color: 'var(--text)', fontSize: '13px' },
  '.cm-scroller': { fontFamily: 'var(--font-mono)', lineHeight: '22px' },
  '.cm-content': { caretColor: 'var(--accent)', padding: '8px 0' },
  '.cm-cursor, .cm-dropCursor': { borderLeftColor: 'var(--accent)' },
  // Sticky: long lines scroll under the gutter, so it stays near-opaque over glass. No blur:
  // the gutter is as tall as the document and moves on every scroll frame.
  '.cm-gutters': {
    backgroundColor: 'var(--sticky-surface)',
    color: 'var(--code-gutter)',
    border: 'none',
    borderRight: '1px solid var(--border-inner)',
  },
  '.cm-activeLine': { backgroundColor: 'var(--zebra)' },
  '.cm-activeLineGutter': { backgroundColor: 'var(--zebra)', color: 'var(--text-muted)' },
  '&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection': { backgroundColor: 'var(--code-selection)' },
  '&.cm-focused': { outline: 'none' },
  '.cm-error-mark': { backgroundColor: 'var(--code-error-mark)', textDecoration: 'underline wavy var(--danger)' },
  '.cm-matchingBracket': { backgroundColor: 'var(--accent-tint)', color: 'inherit' },
  '.cm-tooltip': {
    backgroundColor: 'var(--overlay)',
    border: '1px solid var(--border)',
    color: 'var(--text)',
    boxShadow: 'var(--glass-edge)',
    backdropFilter: 'var(--overlay-blur)',
    WebkitBackdropFilter: 'var(--overlay-blur)',
  },
  '.cm-tooltip-autocomplete > ul > li[aria-selected]': { backgroundColor: 'var(--accent-tint)', color: 'var(--text)' },
  '.cm-panels': { backgroundColor: 'var(--surface-raised)', color: 'var(--text)' },
  '.cm-searchMatch': { backgroundColor: 'var(--warning-tint)' },
});

function language(engine: Engine): Extension[] {
  switch (engine) {
    case 'postgres':
      return [sql({ dialect: PostgreSQL, upperCaseKeywords: true })];
    case 'mysql':
      return [sql({ dialect: MySQL, upperCaseKeywords: true })];
    case 'mariadb':
      return [sql({ dialect: MariaSQL, upperCaseKeywords: true })];
    case 'redis':
      return [redisLanguage];
  }
}

/** The text to run: the selection when there is one, otherwise everything. */
function runTarget(): { text: string; offset: number } {
  if (!view) return { text: props.modelValue, offset: 0 };
  const { from, to } = view.state.selection.main;
  if (to > from) return { text: view.state.sliceDoc(from, to), offset: from };
  return { text: view.state.doc.toString(), offset: 0 };
}

function runFromEditor(): boolean {
  const target = runTarget();
  emit('run', target.text, target.offset);
  return true;
}

/** The Redis command on the cursor's line (`CONFIG GET`), for console help; `null` on an empty line. */
function commandAtCursor(): string | null {
  if (!view) return null;
  const line = view.state.doc.lineAt(view.state.selection.main.head);
  return commandOfLine(line.text);
}

function helpFromEditor(): boolean {
  if (props.engine !== 'redis') return false;
  emit('help', commandAtCursor());
  return true;
}

onMounted(() => {
  if (!host.value) return;
  view = new EditorView({
    parent: host.value,
    state: EditorState.create({
      doc: props.modelValue,
      extensions: [
        Prec.highest(
          keymap.of([
            { key: 'Mod-Enter', run: runFromEditor, preventDefault: true },
            // Redis console help for the command on the cursor's line.
            { key: 'F1', run: helpFromEditor },
          ]),
        ),
        basicSetup,
        ...language(props.engine),
        syntaxHighlighting(highlight),
        theme,
        errorField,
        EditorView.contentAttributes.of({ 'aria-label': props.label, 'aria-multiline': 'true' }),
        EditorView.updateListener.of((update) => {
          if (update.docChanged) emit('update:modelValue', update.state.doc.toString());
        }),
      ],
    }),
  });
  markError(props.errorRange);
});

onBeforeUnmount(() => {
  view?.destroy();
  view = null;
});

watch(
  () => props.revision,
  () => {
    if (!view) return;
    const current = view.state.doc.toString();
    if (current === props.modelValue) return;
    view.dispatch({ changes: { from: 0, to: current.length, insert: props.modelValue } });
  },
);

/**
 * Marks the error of a new run once: underline, scroll and cursor. Typing
 * clears the mark (see `errorField`), and nothing re-marks it.
 */
function markError(range: { key: number; from: number; to: number; doc: string } | null): void {
  if (!view || !range || markedErrorRuns.has(range.key)) return;
  rememberMarkedRun(range.key);
  // Positions refer to the text when the run started; after any edit they would point elsewhere.
  if (view.state.doc.toString() !== range.doc) return;
  const length = view.state.doc.length;
  const from = Math.min(range.from, length);
  const to = Math.min(Math.max(range.to, from), length);
  view.dispatch({
    effects: [setError.of({ from, to }), EditorView.scrollIntoView(from, { y: 'center' })],
    selection: EditorSelection.cursor(from),
  });
}

watch(
  () => props.errorRange?.key ?? null,
  () => {
    // A new run without an error position clears an old mark.
    if (!props.errorRange && view) view.dispatch({ effects: setError.of(null) });
    markError(props.errorRange);
  },
);

function focus(): void {
  view?.focus();
}

defineExpose({ runTarget, focus, commandAtCursor });
</script>

<template>
  <div
    ref="host"
    class="editor"
  />
</template>

<style scoped>
.editor {
  height: 100%;
  min-height: 0;
  overflow: hidden;
}
</style>
