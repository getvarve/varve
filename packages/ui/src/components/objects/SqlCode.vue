<script setup lang="ts">
import { MariaSQL, MySQL, PostgreSQL, sql } from '@codemirror/lang-sql';
import { HighlightStyle, syntaxHighlighting } from '@codemirror/language';
import { Annotation, Compartment, EditorSelection, EditorState } from '@codemirror/state';
import type { Extension } from '@codemirror/state';
import { EditorView } from '@codemirror/view';
import { tags } from '@lezer/highlight';
import { basicSetup } from 'codemirror';
import { computed, inject, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import type { Engine } from '../../api/records';
import { OBJECT_EDITOR_CONTEXT } from './types';

/**
 * SQL text with the engine's dialect highlighting: read only for DDL and
 * plan previews, editable for definitions and bodies (`v-model`). Inside
 * an object editor it is read only while the editor applies a plan.
 */
const props = withDefaults(
  defineProps<{
    modelValue: string;
    engine: Engine;
    /** Accessible name of the text area. */
    label: string;
    readonly?: boolean;
    /** Minimum height in pixels; the editor grows with its container. */
    minHeight?: number;
    /** Id of an element describing the editor (errors, help). */
    describedBy?: string;
  }>(),
  { readonly: false, minHeight: 120, describedBy: '' },
);
const emit = defineEmits<{ 'update:modelValue': [value: string]; cursor: [line: number, column: number] }>();

const host = ref<HTMLElement | null>(null);
let view: EditorView | null = null;
const readOnly = new Compartment();
const attributes = new Compartment();
/** Marks the transactions that copy `modelValue` in, so they are not emitted back. */
const fromModel = Annotation.define<boolean>();
const editor = inject(OBJECT_EDITOR_CONTEXT, null);
const locked = computed(() => props.readonly || (editor?.busy.value ?? false));

/** CodeMirror keeps `\n` line breaks only; text with `\r\n` is the same text. */
function lineFeeds(text: string): string {
  return text.replace(/\r\n?/g, '\n');
}

const highlight = HighlightStyle.define([
  { tag: [tags.keyword, tags.operatorKeyword, tags.modifier], color: 'var(--code-keyword)' },
  { tag: [tags.string, tags.special(tags.string)], color: 'var(--code-string)' },
  { tag: [tags.number, tags.bool, tags.null], color: 'var(--code-number)' },
  { tag: [tags.comment, tags.lineComment, tags.blockComment], color: 'var(--code-comment)', fontStyle: 'italic' },
  { tag: [tags.typeName, tags.standard(tags.name)], color: 'var(--code-keyword)' },
]);

const theme = EditorView.theme({
  '&': { height: '100%', backgroundColor: 'var(--surface)', color: 'var(--text)', fontSize: '12px' },
  '.cm-scroller': { fontFamily: 'var(--font-mono)', lineHeight: '17px' },
  '.cm-content': { caretColor: 'var(--accent)', padding: '8px 0' },
  '.cm-cursor, .cm-dropCursor': { borderLeftColor: 'var(--accent)' },
  // Sticky: long lines scroll under the gutter, so it stays near-opaque over glass.
  '.cm-gutters': {
    backgroundColor: 'var(--sticky)',
    color: 'var(--code-gutter)',
    border: 'none',
    borderRight: '1px solid var(--border-inner)',
  },
  '.cm-activeLine': { backgroundColor: 'transparent' },
  '.cm-activeLineGutter': { backgroundColor: 'transparent', color: 'var(--text-muted)' },
  '&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection': { backgroundColor: 'var(--code-selection)' },
  '&.cm-focused': { outline: 'none' },
  '.cm-matchingBracket': { backgroundColor: 'var(--accent-tint)', color: 'inherit' },
  '.cm-tooltip': {
    backgroundColor: 'var(--overlay)',
    border: '1px solid var(--border)',
    color: 'var(--text)',
    boxShadow: 'var(--glass-edge)',
    backdropFilter: 'var(--overlay-blur)',
    WebkitBackdropFilter: 'var(--overlay-blur)',
  },
  '.cm-panels': { backgroundColor: 'var(--surface-raised)', color: 'var(--text)' },
  '.cm-searchMatch': { backgroundColor: 'var(--warning-tint)' },
});

function language(engine: Engine): Extension {
  switch (engine) {
    case 'postgres':
      return sql({ dialect: PostgreSQL, upperCaseKeywords: true });
    case 'mariadb':
      return sql({ dialect: MariaSQL, upperCaseKeywords: true });
    default:
      return sql({ dialect: MySQL, upperCaseKeywords: true });
  }
}

function contentAttributes(): Extension {
  const attrs: Record<string, string> = { 'aria-label': props.label, 'aria-multiline': 'true' };
  if (locked.value) attrs['aria-readonly'] = 'true';
  if (props.describedBy) attrs['aria-describedby'] = props.describedBy;
  return EditorView.contentAttributes.of(attrs);
}

onMounted(() => {
  if (!host.value) return;
  view = new EditorView({
    parent: host.value,
    state: EditorState.create({
      doc: props.modelValue,
      extensions: [
        basicSetup,
        language(props.engine),
        syntaxHighlighting(highlight),
        theme,
        readOnly.of(EditorState.readOnly.of(locked.value)),
        attributes.of(contentAttributes()),
        EditorView.updateListener.of((update) => {
          if (update.docChanged && !update.transactions.some((t) => t.annotation(fromModel))) emit('update:modelValue', update.state.doc.toString());
          if (update.selectionSet || update.docChanged) {
            const head = update.state.selection.main.head;
            const line = update.state.doc.lineAt(head);
            emit('cursor', line.number, head - line.from + 1);
          }
        }),
      ],
    }),
  });
});

onBeforeUnmount(() => {
  view?.destroy();
  view = null;
});

watch(
  () => props.modelValue,
  (value) => {
    if (!view) return;
    const current = view.state.doc.toString();
    if (current !== lineFeeds(value)) view.dispatch({ changes: { from: 0, to: current.length, insert: value }, annotations: fromModel.of(true) });
  },
);

watch(
  () => [locked.value, props.label, props.describedBy] as const,
  () => {
    view?.dispatch({
      effects: [readOnly.reconfigure(EditorState.readOnly.of(locked.value)), attributes.reconfigure(contentAttributes())],
    });
  },
);

/** Scrolls to a 1-based line and puts the cursor at its start. */
function goToLine(line: number): void {
  if (!view) return;
  const target = view.state.doc.line(Math.min(Math.max(1, line), view.state.doc.lines));
  view.dispatch({ selection: EditorSelection.cursor(target.from), effects: EditorView.scrollIntoView(target.from, { y: 'start' }) });
  view.focus();
}

function focus(): void {
  view?.focus();
}

defineExpose({ goToLine, focus });
</script>

<template>
  <div
    ref="host"
    class="code"
    :style="{ minHeight: `${minHeight}px` }"
  />
</template>

<style scoped>
.code {
  height: 100%;
  min-height: 0;
  overflow: hidden;
  border: 1px solid var(--border-inner);
  border-radius: var(--radius-control);
}

/* CodeMirror drops its own outline: show keyboard focus with the same ring as the other controls. */
.code:focus-within {
  outline: 2px solid var(--accent);
  outline-offset: 2px;
}
</style>
