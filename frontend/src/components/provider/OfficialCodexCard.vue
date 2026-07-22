<script setup lang="ts">
import { t } from '@/i18n'
import AppButton from '@/components/ui/AppButton.vue'
import IconBot from '~icons/lucide/bot'
import IconCheck from '~icons/lucide/check'
import IconLogIn from '~icons/lucide/log-in'
import IconPlay from '~icons/lucide/play'
import IconX from '~icons/lucide/x'

defineProps<{
  active: boolean
  loggedIn: boolean
  loginRunning: boolean
  busy: boolean
}>()

defineEmits<{ enable: []; login: []; cancel: [] }>()
</script>

<template>
  <article class="official-card" :class="{ 'official-card--active': active }">
    <span class="official-card__logo" aria-hidden="true"><IconBot /></span>
    <div class="official-card__main">
      <strong class="official-card__name">{{ t('providers.officialName') }}</strong>
      <span class="official-card__url">{{ t('providers.officialDirect') }}</span>
      <span class="official-card__status">
        {{ loggedIn ? t('providers.officialAccountReady') : t('providers.officialNotLoggedIn') }}
      </span>
    </div>
    <div class="official-card__actions">
      <AppButton
        v-if="loginRunning"
        variant="danger"
        size="sm"
        :icon="IconX"
        :label="t('providers.officialCancelLogin')"
        @click="$emit('cancel')"
      />
      <AppButton
        v-else-if="!loggedIn"
        variant="primary"
        size="sm"
        :icon="IconLogIn"
        :label="t('providers.officialLogin')"
        :disabled="busy"
        @click="$emit('login')"
      />
      <AppButton
        v-else-if="active"
        variant="secondary"
        size="sm"
        :icon="IconCheck"
        :label="t('providers.enabled')"
        disabled
      />
      <AppButton
        v-else
        variant="primary"
        size="sm"
        :icon="IconPlay"
        :label="t('providers.enable')"
        :disabled="busy"
        @click="$emit('enable')"
      />
    </div>
  </article>
</template>

<style scoped>
.official-card {
  display: flex;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-3) var(--space-4);
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: var(--radius-lg);
  transition: border-color var(--transition), box-shadow var(--transition);
}
.official-card--active {
  border-color: var(--accent);
  box-shadow: inset 0 0 0 1px var(--accent);
}
.official-card__logo {
  width: 32px;
  height: 32px;
  display: grid;
  place-items: center;
  flex-shrink: 0;
  border-radius: var(--radius);
  background: var(--surface-2);
  color: var(--text);
}
.official-card__logo :deep(svg) {
  width: 19px;
  height: 19px;
}
.official-card__main {
  min-width: 0;
  flex: 1;
  display: flex;
  flex-direction: column;
}
.official-card__name {
  font-size: var(--fs-md);
  font-weight: 600;
}
.official-card__url,
.official-card__status {
  color: var(--text-muted);
  font-size: var(--fs-sm);
}
.official-card__actions {
  display: flex;
  flex-shrink: 0;
}
</style>
