<script setup lang="ts">
import { MariaSQL, MySQL, PostgreSQL, sql } from '@codemirror/lang-sql';
import { HighlightStyle, syntaxHighlighting } from '@codemirror/language';
import { Compartment, EditorSelection, EditorState } from '@codemirror/state';
import type { Extension } from '@codemirror/state';
import { EditorView } from '@codemirror/view';
import { tags } from '@lezer/highlight';
import { basicSetup } from 'codemirror';
import { onBeforeUnmount, onMounted, ref, watch } from 'vue';
import type { Engine } from '../../api/records';

/**
 * SQL text with the engine's dialect highlighting: read only for DDL and
 * plan previews, editable for definitions and bodies (`v-model`).
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
  '.cm-gutters': {
    backgroundColor: 'var(--surface-raised)',
    color: 'var(--code-gutter)',
    border: 'none',
    borderRight: '1px solid var(--border-inner)',
  },
  '.cm-activeLine': { backgroundColor: 'transparent' },
  '.cm-activeLineGutter': { backgroundColor: 'transparent', color: 'var(--text-muted)' },
  '&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection': { backgroundColor: 'var(--code-selection)' },
  '&.cm-focused': { outline: 'none' },
  '.cm-matchingBracket': { backgroundColor: 'var(--accent-tint)', color: 'inherit' },
  '.cm-tooltip': { backgroundColor: 'var(--surface)', border: '1px solid var(--border)', color: 'var(--text)' },
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
  if (props.readonly) attrs['aria-readonly'] = 'true';
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
        readOnly.of(EditorState.readOnly.of(props.readonly)),
        attributes.of(contentAttributes()),
        EditorView.updateListener.of((update) => {
          if (update.docChanged) emit('update:modelValue', update.state.doc.toString());
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
    if (current !== value) view.dispatch({ changes: { from: 0, to: current.length, insert: value } });
  },
);

watch(
  () => [props.readonly, props.label, props.describedBy] as const,
  () => {
    view?.dispatch({
      effects: [readOnly.reconfigure(EditorState.readOnly.of(props.readonly)), attributes.reconfigure(contentAttributes())],
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
</style>
