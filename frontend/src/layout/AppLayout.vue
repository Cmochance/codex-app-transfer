<script setup lang="ts">
import AppSidebar from './AppSidebar.vue'
import AppTitlebar from './AppTitlebar.vue'
</script>

<template>
  <div class="app-shell">
    <AppSidebar />
    <div class="app-shell__main">
      <AppTitlebar />
      <main class="app-shell__content">
        <RouterView />
      </main>
    </div>
  </div>
</template>

<style scoped>
.app-shell {
  display: grid;
  grid-template-columns: var(--sidebar-w) 1fr;
  height: 100vh;
  overflow: hidden;
}
.app-shell__main {
  display: flex;
  flex-direction: column;
  min-width: 0;
  background: var(--bg);
}
.app-shell__content {
  flex: 1;
  overflow-y: auto;
}
</style>
