<script setup lang="ts">
import { computed, useId } from 'vue';
import { formatProgress, progressPercent, useUpdates } from '../../composables/useUpdates';
import Icon from '../common/Icon.vue';
import Modal from '../common/Modal.vue';

/**
 * The update flow of the `tauri` mechanism (Windows and Linux): checking,
 * the release notes of a found version, the download, and the install. On
 * macOS Sparkle shows its own windows instead.
 */
const updates = useUpdates();
const messageId = useId();

const status = computed(() => updates.state.status);
const currentVersion = computed(() => updates.state.info?.currentVersion ?? '');

const title = computed(() => {
  const s = status.value;
  switch (s.kind) {
    case 'upToDate':
      return 'Varve is up to date';
    case 'available':
      return 'A new version of Varve is available';
    case 'downloading':
      return `Downloading Varve ${s.update.version}`;
    case 'ready':
      return `Varve ${s.update.version} is ready to install`;
    case 'error':
      return s.update ? 'Could not update Varve' : 'Could not check for updates';
    default:
      return 'Checking for updates…';
  }
});

const percent = computed(() => (status.value.kind === 'downloading' ? progressPercent(status.value.progress) : null));
</script>

<template>
  <Modal
    :title="title"
    :width="520"
    :described-by="messageId"
    @close="updates.closeDialog()"
  >
    <div
      v-if="status.kind === 'checking' || status.kind === 'idle'"
      class="stack"
    >
      <p
        :id="messageId"
        class="muted"
      >
        Looking for a newer version of Varve.
      </p>
      <div
        class="meter indeterminate"
        role="progressbar"
        aria-label="Checking for updates"
      >
        <span />
      </div>
    </div>

    <p
      v-else-if="status.kind === 'upToDate'"
      :id="messageId"
    >
      Varve {{ currentVersion }} is the newest version.
    </p>

    <div
      v-else-if="status.kind === 'available'"
      class="stack"
    >
      <p :id="messageId">
        Varve {{ status.update.version }} is available; you have {{ currentVersion }}.
      </p>
      <template v-if="status.update.notes">
        <span class="label">Release notes</span>
        <pre
          class="notes"
          tabindex="0"
          aria-label="Release notes"
        >{{ status.update.notes }}</pre>
      </template>
    </div>

    <div
      v-else-if="status.kind === 'downloading'"
      class="stack"
    >
      <p
        :id="messageId"
        class="muted"
      >
        The download continues when you close this window.
      </p>
      <div
        class="meter"
        :class="{ indeterminate: percent === null }"
        role="progressbar"
        aria-label="Downloading the update"
        :aria-valuenow="percent ?? undefined"
        :aria-valuemin="percent === null ? undefined : 0"
        :aria-valuemax="percent === null ? undefined : 100"
        :aria-valuetext="formatProgress(status.progress)"
      >
        <span :style="percent === null ? undefined : { width: `${percent}%` }" />
      </div>
      <span class="size mono">{{ formatProgress(status.progress) }}</span>
    </div>

    <p
      v-else-if="status.kind === 'ready'"
      :id="messageId"
    >
      Varve restarts to install the update. You are asked about unsaved work first.
    </p>

    <p
      v-else-if="status.kind === 'error'"
      :id="messageId"
      class="notice tone-danger"
      role="alert"
    >
      <Icon name="warning" />
      <span>{{ status.message }}</span>
    </p>

    <template #footer>
      <template v-if="status.kind === 'available'">
        <button
          type="button"
          class="btn skip"
          @click="updates.skipVersion()"
        >
          Skip This Version
        </button>
        <button
          type="button"
          class="btn"
          @click="updates.closeDialog()"
        >
          Remind Me Later
        </button>
        <button
          type="button"
          class="btn primary"
          data-autofocus
          @click="updates.download()"
        >
          Install Update
        </button>
      </template>
      <template v-else-if="status.kind === 'ready'">
        <button
          type="button"
          class="btn"
          @click="updates.closeDialog()"
        >
          Later
        </button>
        <button
          type="button"
          class="btn primary"
          data-autofocus
          :disabled="updates.state.installing"
          @click="updates.installAndRelaunch()"
        >
          Install and Relaunch
        </button>
      </template>
      <button
        v-else-if="status.kind === 'checking' || status.kind === 'idle'"
        type="button"
        class="btn"
        @click="updates.closeDialog()"
      >
        Cancel
      </button>
      <button
        v-else-if="status.kind === 'downloading'"
        type="button"
        class="btn"
        @click="updates.closeDialog()"
      >
        Hide
      </button>
      <button
        v-else
        type="button"
        class="btn primary"
        data-autofocus
        @click="updates.closeDialog()"
      >
        OK
      </button>
    </template>
  </Modal>
</template>

<style scoped>
p {
  margin: 0;
}

.stack {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.notes {
  max-height: 240px;
  margin: 0;
  padding: 10px 12px;
  overflow: auto;
  border: 1px solid var(--border-inner);
  border-radius: var(--radius-control);
  background: var(--inset);
  font-family: var(--font-mono);
  font-size: 12px;
  line-height: 1.5;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}

.notice {
  align-items: flex-start;
}

.meter {
  position: relative;
  display: block;
  height: 6px;
  border-radius: 3px;
  background: var(--surface-raised);
  overflow: hidden;
}

.meter > span {
  display: block;
  width: 0;
  height: 100%;
  border-radius: 3px;
  background: var(--accent);
  transition: width var(--dur-fast) var(--ease-out);
}

/* No known size: a segment slides along instead of filling. */
.meter.indeterminate > span {
  position: absolute;
  width: 30%;
  animation: slide 1.2s var(--ease-in-out) infinite;
}

@keyframes slide {
  from {
    left: -30%;
  }

  to {
    left: 100%;
  }
}

@media (prefers-reduced-motion: reduce) {
  .meter > span {
    transition: none;
  }

  .meter.indeterminate > span {
    left: 0;
    width: 100%;
    opacity: 0.4;
    animation: none;
  }
}

.size {
  font-size: 12px;
  color: var(--text-muted);
}

/* The skip choice stands apart from the two that keep the update. */
.skip {
  margin-right: auto;
}
</style>
