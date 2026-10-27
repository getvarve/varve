<script setup lang="ts">
import { computed } from 'vue';
import type { ObjectKind } from '../../api/catalog';
import type { BrowserAction } from '../../lib/tree';

/** Stroke icons (24 grid) for object kinds and browser tools in the object tree. Always decorative. */
const PATHS = {
  database: 'M4 6c0-1.7 3.6-3 8-3s8 1.3 8 3-3.6 3-8 3-8-1.3-8-3zM4 6v12c0 1.7 3.6 3 8 3s8-1.3 8-3V6M4 12c0 1.7 3.6 3 8 3s8-1.3 8-3',
  schema: 'M4 5h6v6H4zM14 13h6v6h-6zM7 11v5h7',
  table: 'M4 5h16v14H4zM4 10h16M4 14.5h16M10 10v9',
  partition: 'M4 5h16v14H4zM4 10h16M12 10v9M8 14.5h.01M16 14.5h.01',
  view: 'M2 12s4-7 10-7 10 7 10 7-4 7-10 7S2 12 2 12zM12 9a3 3 0 1 0 0 6a3 3 0 1 0 0-6',
  matview: 'M2 12s4-7 10-7 10 7 10 7-4 7-10 7S2 12 2 12zM9 12h6M12 9v6',
  column: 'M9 4h6v16H9z',
  index: 'M5 4v16M5 8h8M5 12h12M5 16h6',
  constraint: 'M12 3l8 3v6c0 5-3.5 8-8 9-4.5-1-8-4-8-9V6z',
  sequence: 'M5 7h2v10M11 9a2 2 0 1 1 4 0c0 2-4 4-4 8h4M19 7v10',
  routine: 'M10 4c-2 0-2 2-2 4v8c0 2-1 4-3 4M5 10h6M14 9l6 6M20 9l-6 6',
  trigger: 'M13 3L5 13h6l-1 8 8-10h-6z',
  type: 'M5 6h14M12 6v13M8 19h8',
  extension: 'M9 4h6v3a2 2 0 1 0 0 4v0h3v9H6v-9h3a2 2 0 1 0 0-4V4z',
  rule: 'M5 4h14v16H5zM8 8h8M8 12h8M8 16h5',
  policy: 'M5 11h14v10H5zM8 11V7a4 4 0 0 1 8 0v4',
  publication: 'M4 12a8 8 0 0 1 16 0M8 12a4 4 0 0 1 8 0M12 12v8',
  foreign: 'M12 3a9 9 0 1 0 0 18a9 9 0 1 0 0-18M3 12h18M12 3c3 3 3 15 0 18M12 3c-3 3-3 15 0 18',
  tablespace: 'M3 7h18v10H3zM7 12h.01M11 12h6',
  event: 'M4 6h16v14H4zM4 10h16M8 3v4M16 3v4M12 13v3l2 1',
  package: 'M4 8l8-4 8 4v8l-8 4-8-4zM4 8l8 4 8-4M12 12v8',
  operator: 'M6 12h12M12 6v12',
  panel: 'M3 12h4l3-8 4 16 3-8h4',
  server: 'M4 4h16v6H4zM4 14h16v6H4zM8 7h.01M8 17h.01',
  key: 'M3 15a4 4 0 1 0 8 0a4 4 0 1 0-8 0M10 12l10-10M16 6l3 3M13 9l2 2',
  terminal: 'M4 17l6-5-6-5M12 19h8',
  bell: 'M6 16V11a6 6 0 0 1 12 0v5l2 2H4zM10 20a2 2 0 0 0 4 0',
  broadcast: 'M12 12h.01M8.5 8.5a5 5 0 0 0 0 7M15.5 8.5a5 5 0 0 1 0 7M5.6 5.6a9 9 0 0 0 0 12.8M18.4 5.6a9 9 0 0 1 0 12.8',
} as const;

type Glyph = keyof typeof PATHS;

const KIND_GLYPHS: Record<ObjectKind, Glyph> = {
  database: 'database',
  schema: 'schema',
  table: 'table',
  partitionedTable: 'partition',
  partition: 'partition',
  view: 'view',
  materializedView: 'matview',
  column: 'column',
  index: 'index',
  constraint: 'constraint',
  sequence: 'sequence',
  function: 'routine',
  procedure: 'routine',
  aggregate: 'routine',
  trigger: 'trigger',
  eventTrigger: 'trigger',
  enumType: 'type',
  compositeType: 'type',
  domain: 'type',
  rangeType: 'type',
  extension: 'extension',
  rule: 'rule',
  policy: 'policy',
  publication: 'publication',
  subscription: 'publication',
  foreignDataWrapper: 'foreign',
  foreignServer: 'foreign',
  foreignTable: 'foreign',
  tablespace: 'tablespace',
  collation: 'type',
  cast: 'operator',
  operator: 'operator',
  event: 'event',
  package: 'package',
  packageBody: 'package',
};

const ACTION_GLYPHS: Record<BrowserAction, Glyph> = { keys: 'key', console: 'terminal', notify: 'bell', pubsub: 'broadcast' };

/** One of `kind`, `action` or `server`; none draws the live panel glyph. */
const props = defineProps<{ kind?: ObjectKind; action?: BrowserAction; server?: boolean }>();

const d = computed(() => {
  if (props.kind) return PATHS[KIND_GLYPHS[props.kind]];
  if (props.action) return PATHS[ACTION_GLYPHS[props.action]];
  if (props.server) return PATHS.server;
  return PATHS.panel;
});
</script>

<template>
  <svg
    class="object-icon"
    width="14"
    height="14"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-width="2"
    stroke-linecap="round"
    stroke-linejoin="round"
    aria-hidden="true"
    focusable="false"
  >
    <path :d="d" />
  </svg>
</template>

<style scoped>
.object-icon {
  flex-shrink: 0;
}
</style>
