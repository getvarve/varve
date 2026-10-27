<script setup lang="ts">
import { computed, ref, shallowRef } from 'vue';
import type { CmdError } from '../../api/errors';
import { anyDriverErrorOf } from '../../api/errors';
import { redisModules, redisSearchIndex, redisSearchIndexes } from '../../api/redis';
import type { ModuleFeature, ModuleReport, SearchIndex } from '../../api/redis';
import type { SessionInfo } from '../../api/sessions';
import { latestOnly } from '../../lib/async';
import { cellText, formatCount } from '../../lib/values';
import EnvBadge from '../common/EnvBadge.vue';
import LiveError from './LiveError.vue';
import { useReporter } from './report';

/**
 * Redis modules (design doc 4.2, "JSON, Search, TimeSeries, Bloom":
 * browse): which module features this server has (each missing one with the
 * reason), the loaded modules, and the Search indexes with their definition
 * rebuilt as an `FT.CREATE` command. Read only; module keys open in the key
 * browser.
 */
const props = defineProps<{
  /** The tab's Redis session. */
  session: SessionInfo;
  /** The workspace tab the panel runs in; its errors are reported to the tab. */
  tabId?: string | null;
}>();
const emit = defineEmits<{ reconnect: [] }>();
const report = useReporter(() => props.tabId);

const FEATURE_LABELS: Record<ModuleFeature, string> = {
  json: 'JSON',
  search: 'Search',
  timeSeries: 'TimeSeries',
  bloom: 'Bloom filters',
  cuckoo: 'Cuckoo filters',
  countMin: 'Count-Min sketches',
  topK: 'Top-K',
  tDigest: 't-digest',
};

const modules = shallowRef<ModuleReport | null>(null);
const indexes = shallowRef<string[] | null>(null);
const selected = ref<string | null>(null);
const index = shallowRef<SearchIndex | null>(null);
const indexMissing = ref(false);
const error = shallowRef<CmdError | null>(null);
const loading = ref(false);
const loadingIndex = ref(false);
const requests = latestOnly();

const searchAvailable = computed(() => modules.value?.features.some((f) => f.feature === 'search' && f.available) ?? false);

async function load(): Promise<void> {
  loading.value = true;
  error.value = null;
  try {
    const report = await redisModules(props.session.id);
    modules.value = report;
    indexes.value = report.features.some((f) => f.feature === 'search' && f.available) ? await redisSearchIndexes(props.session.id) : null;
    if (selected.value && !(indexes.value ?? []).includes(selected.value)) {
      selected.value = null;
      index.value = null;
    }
  } catch (e: unknown) {
    error.value = report(e);
  } finally {
    loading.value = false;
  }
}

async function open(name: string): Promise<void> {
  const token = requests.next();
  selected.value = name;
  index.value = null;
  indexMissing.value = false;
  loadingIndex.value = true;
  try {
    const found = await redisSearchIndex(props.session.id, name);
    if (!requests.isCurrent(token)) return;
    index.value = found;
    indexMissing.value = found === null;
  } catch (e: unknown) {
    if (requests.isCurrent(token)) error.value = report(e);
  } finally {
    if (requests.isCurrent(token)) loadingIndex.value = false;
  }
}

const copied = ref(false);

/** Copies the one-line FT.CREATE command, which the console and redis-cli run as is. */
async function copyDdl(): Promise<void> {
  const ddl = index.value?.ddl;
  if (!ddl) return;
  try {
    await navigator.clipboard.writeText(ddl);
    copied.value = true;
    setTimeout(() => {
      copied.value = false;
    }, 2000);
  } catch {
    // The clipboard can be unavailable; the text stays selectable.
  }
}

/** A search error that only says the feature is missing is shown in the feature list instead. */
const shownError = computed(() => {
  const e = error.value;
  if (!e) return null;
  return anyDriverErrorOf(e)?.code === 'SEARCH_UNAVAILABLE' ? null : e;
});

void load();
</script>

<template>
  <div class="panel">
    <div class="head">
      <h1>Modules</h1>
      <span class="source">COMMAND INFO · MODULE LIST · FT._LIST</span>
      <EnvBadge :environment="session.environment" />
      <span class="grow" />
      <button
        type="button"
        class="btn compact"
        :disabled="loading"
        @click="load"
      >
        Refresh
      </button>
    </div>
    <LiveError
      v-if="shownError"
      :error="shownError"
      class="panel-error"
      @reconnect="emit('reconnect')"
    />

    <div class="scroll body">
      <section
        v-if="modules"
        class="box"
        aria-label="Module features"
      >
        <h2>Features</h2>
        <ul class="features">
          <li
            v-for="f in modules.features"
            :key="f.feature"
          >
            <span
              class="pill"
              :class="f.available ? 'tone-success' : 'tone-neutral'"
            >{{ f.available ? 'available' : 'not available' }}</span>
            <strong>{{ FEATURE_LABELS[f.feature] }}</strong>
            <span
              v-if="f.reason"
              class="subtle"
            >{{ f.reason }}</span>
          </li>
        </ul>
        <p
          v-if="modules.modules === null"
          class="subtle note"
        >
          This user may not run MODULE LIST, so the module versions are not shown.
        </p>
        <p
          v-else-if="modules.modules.length > 0"
          class="subtle note mono"
        >
          Loaded: {{ modules.modules.map((m) => (m.version ? `${m.name} ${m.version}` : m.name)).join(', ') }}
        </p>
        <p class="subtle note">
          Module keys (time series, Bloom and Cuckoo filters, Count-Min sketches, Top-K, t-digest, JSON) open in the key browser.
        </p>
      </section>

      <section
        v-if="searchAvailable"
        class="box"
        aria-label="Search indexes"
      >
        <h2>Search indexes</h2>
        <p class="subtle note">
          Search indexes cover the keys of database 0. Varve shows them read only.
        </p>
        <p
          v-if="indexes && indexes.length === 0"
          class="subtle"
        >
          No indexes.
        </p>
        <div
          v-else-if="indexes"
          class="split"
        >
          <ul
            class="index-list"
            aria-label="Indexes"
          >
            <li
              v-for="name in indexes"
              :key="name"
            >
              <button
                type="button"
                class="index mono"
                :class="{ active: selected === name }"
                :aria-pressed="selected === name"
                @click="open(name)"
              >
                {{ name }}
              </button>
            </li>
          </ul>
          <div class="detail">
            <p
              v-if="loadingIndex"
              class="subtle"
              role="status"
            >
              Reading the index…
            </p>
            <p
              v-else-if="indexMissing"
              class="notice tone-warning"
            >
              This index no longer exists.
            </p>
            <template v-else-if="index">
              <dl class="facts">
                <div>
                  <dt>On</dt>
                  <dd class="mono">
                    {{ index.keyType ?? 'unknown' }}
                  </dd>
                </div>
                <div>
                  <dt>Prefixes</dt>
                  <dd class="mono">
                    {{ index.prefixes.length > 0 ? index.prefixes.join(', ') : 'every key' }}
                  </dd>
                </div>
                <div v-if="index.numDocs !== null">
                  <dt>Documents</dt>
                  <dd class="mono">
                    {{ formatCount(index.numDocs) }}
                  </dd>
                </div>
                <div v-if="index.indexingFailures !== null">
                  <dt>Failures</dt>
                  <dd class="mono">
                    {{ formatCount(index.indexingFailures) }}
                  </dd>
                </div>
                <div v-if="index.percentIndexed !== null && index.percentIndexed < 1">
                  <dt>Indexed</dt>
                  <dd class="mono">
                    {{ Math.round(index.percentIndexed * 100) }}%
                  </dd>
                </div>
              </dl>
              <div class="ddl-head">
                <h3>{{ index.ddlDisplay ? 'Definition' : 'FT.INFO reply' }}</h3>
                <span class="grow" />
                <button
                  v-if="index.ddl"
                  type="button"
                  class="btn compact"
                  title="Copies the command as one line, ready for the console or redis-cli"
                  @click="copyDdl"
                >
                  {{ copied ? 'Copied' : 'Copy command' }}
                </button>
              </div>
              <pre
                v-if="index.ddlDisplay"
                class="mono ddl"
                aria-label="Index definition"
              >{{ index.ddlDisplay }}</pre>
              <pre
                v-else
                class="mono ddl"
                aria-label="Raw FT.INFO reply"
              >{{ index.raw }}</pre>
              <p
                v-for="n in index.notes"
                :key="n"
                class="subtle note"
              >
                {{ n }}
              </p>
              <h3>Attributes</h3>
              <table class="table">
                <thead>
                  <tr>
                    <th>Field</th>
                    <th>Name</th>
                    <th>Type</th>
                    <th>Options</th>
                  </tr>
                </thead>
                <tbody>
                  <tr
                    v-for="a in index.attributes"
                    :key="a.identifier"
                  >
                    <td class="mono">
                      {{ a.identifier }}
                    </td>
                    <td class="mono">
                      {{ a.attribute }}
                    </td>
                    <td class="mono">
                      {{ a.fieldType }}
                    </td>
                    <td class="mono">
                      {{ a.options.join(' ') }}
                    </td>
                  </tr>
                </tbody>
              </table>
              <details>
                <summary>Statistics</summary>
                <table class="table">
                  <tbody>
                    <tr
                      v-for="s in index.stats"
                      :key="s.name"
                    >
                      <th>{{ s.name }}</th>
                      <td class="mono">
                        {{ cellText(s.value) }}
                      </td>
                    </tr>
                  </tbody>
                </table>
              </details>
            </template>
            <p
              v-else
              class="subtle"
            >
              Choose an index to see its definition.
            </p>
          </div>
        </div>
      </section>
      <p
        v-if="loading && !modules"
        class="subtle"
        role="status"
      >
        Reading the modules…
      </p>
    </div>
  </div>
</template>

<style scoped>
.panel-error {
  margin: 8px 16px 0;
}

.body {
  display: flex;
  flex-direction: column;
  gap: 12px;
  padding: 16px;
  background: var(--ground);
}

.box {
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 10px 12px;
  border: 1px solid var(--border);
  border-radius: var(--radius-panel);
  background: var(--surface);
}

h2,
h3 {
  margin: 0;
  font-size: 13px;
  font-weight: 600;
}

.features {
  display: flex;
  flex-direction: column;
  gap: 4px;
  margin: 0;
  padding: 0;
  list-style: none;
  font-size: 12px;
}

.features li {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
}

.note {
  margin: 0;
  font-size: 12px;
}

.split {
  display: flex;
  gap: 12px;
  min-height: 0;
}

.index-list {
  flex: 0 0 220px;
  max-height: 480px;
  margin: 0;
  padding: 0;
  overflow: auto;
  list-style: none;
}

.index {
  width: 100%;
  padding: 4px 8px;
  border: 0;
  border-radius: var(--radius-control);
  background: transparent;
  color: var(--text);
  text-align: left;
  cursor: pointer;
}

.index.active {
  background: var(--accent-tint);
}

.detail {
  display: flex;
  flex: 1;
  flex-direction: column;
  gap: 8px;
  min-width: 0;
  font-size: 12px;
}

.facts {
  display: flex;
  flex-wrap: wrap;
  gap: 16px;
  margin: 0;
}

.facts dt {
  color: var(--text-muted);
  font-size: 11px;
}

.facts dd {
  margin: 0;
}

.ddl-head {
  display: flex;
  align-items: center;
}

.grow {
  flex: 1;
}

.ddl {
  margin: 0;
  padding: 8px;
  border-radius: var(--radius-control);
  background: var(--zebra);
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}

.table {
  border-collapse: collapse;
  font-size: 12px;
}

.table th {
  text-align: left;
  font-weight: 500;
  color: var(--text-muted);
  padding: 3px 8px;
}

.table td {
  padding: 3px 8px;
  border-bottom: 1px solid var(--border);
}

.compact {
  height: 28px;
  font-size: 12px;
}
</style>
