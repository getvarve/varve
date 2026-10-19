<script setup lang="ts">
const proFeatures = [
  { title: 'Statistics history', text: 'A snapshot on every connect, kept forever, with trends since last connect, 30 days and all time.' },
  { title: 'Structure Sync, Data Sync, Data Transfer', text: 'Compare and synchronise schemas and data between servers, and move data across engines.' },
  { title: 'Cloud sync and team workspaces', text: 'Saved queries and connections on all your devices, shared with your team. Passwords never leave your device.' },
  { title: 'Wrapped', text: 'A yearly recap of your year with your databases.' },
];
</script>

<template>
  <section class="about">
    <h1>About Varve Pro</h1>
    <p class="lede">
      You are using Varve Community: free, open source and complete on its own. Varve Pro is the same app with
      extra extensions.
    </p>
    <ul class="features">
      <li
        v-for="feature in proFeatures"
        :key="feature.title"
      >
        <h2>{{ feature.title }}</h2>
        <p>{{ feature.text }}</p>
      </li>
    </ul>
    <p class="note">
      Community and Pro share the same local data, so you can switch at any time without losing your connections or
      history.
    </p>
  </section>
</template>

<style scoped>
.about {
  display: flex;
  flex-direction: column;
  gap: 16px;
  max-width: 720px;
  padding: 48px;
}

h1 {
  margin: 0;
  font-size: 24px;
  font-weight: 600;
}

.lede {
  margin: 0;
  font-size: 15px;
  color: var(--text-muted);
}

.features {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(260px, 1fr));
  gap: 12px;
  margin: 0;
  padding: 0;
  list-style: none;
}

.features li {
  padding: 16px;
  border: 1px solid var(--border);
  border-radius: var(--radius-panel);
  background: var(--surface);
}

h2 {
  margin: 0 0 4px;
  font-size: 15px;
  font-weight: 500;
}

.features p,
.note {
  margin: 0;
  color: var(--text-muted);
}
</style>
