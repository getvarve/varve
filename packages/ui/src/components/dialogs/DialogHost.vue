<script setup lang="ts">
import { useDialogs } from '../../composables/useDialogs';
import ConfirmDialog from './ConfirmDialog.vue';
import ConnectionEditor from './ConnectionEditor.vue';
import ConnectionPicker from './ConnectionPicker.vue';
import DeleteFolderDialog from './DeleteFolderDialog.vue';
import DestructiveConfirm from './DestructiveConfirm.vue';
import HostKeyDialog from './HostKeyDialog.vue';
import MoveDialog from './MoveDialog.vue';
import SaveQueryDialog from './SaveQueryDialog.vue';
import SecretPrompt from './SecretPrompt.vue';
import UnsavedCloseDialog from './UnsavedCloseDialog.vue';

/** Renders every open dialog; the last one is on top. */
const { dialogs, close } = useDialogs();
</script>

<template>
  <template
    v-for="d in dialogs"
    :key="d.id"
  >
    <SecretPrompt
      v-if="d.spec.type === 'secret'"
      :profile="d.spec.profile"
      :secret="d.spec.secret"
      :reason="d.spec.reason"
      @done="close(d.id, $event)"
    />
    <HostKeyDialog
      v-else-if="d.spec.type === 'hostKey'"
      :server="d.spec.server"
      :code="d.spec.code"
      :fingerprints="d.spec.fingerprints"
      @done="close(d.id, $event)"
    />
    <DestructiveConfirm
      v-else-if="d.spec.type === 'destructive'"
      :connection-name="d.spec.connectionName"
      :environment="d.spec.environment"
      :statements="d.spec.statements"
      :warnings="d.spec.warnings ?? []"
      @done="close(d.id, $event)"
    />
    <ConfirmDialog
      v-else-if="d.spec.type === 'confirm'"
      :title="d.spec.title"
      :message="d.spec.message"
      :confirm-label="d.spec.confirmLabel"
      :danger="d.spec.danger"
      @done="close(d.id, $event)"
    />
    <DeleteFolderDialog
      v-else-if="d.spec.type === 'deleteFolder'"
      :folder="d.spec.folder"
      :contents="d.spec.contents"
      @done="close(d.id, $event)"
    />
    <ConnectionEditor
      v-else-if="d.spec.type === 'connectionEditor'"
      :profile="d.spec.profile"
      :engine="d.spec.engine"
      :folder-id="d.spec.folderId"
      @done="close(d.id, $event)"
    />
    <SaveQueryDialog
      v-else-if="d.spec.type === 'saveQuery'"
      :request="d.spec.request"
      @done="close(d.id, $event)"
    />
    <MoveDialog
      v-else-if="d.spec.type === 'moveTo'"
      :node="d.spec.node"
      @done="close(d.id, $event)"
    />
    <UnsavedCloseDialog
      v-else-if="d.spec.type === 'unsavedClose'"
      :title="d.spec.title"
      :saved="d.spec.saved"
      @done="close(d.id, $event)"
    />
    <ConnectionPicker
      v-else-if="d.spec.type === 'pickConnection'"
      :title="d.spec.title"
      @done="close(d.id, $event)"
    />
  </template>
</template>
