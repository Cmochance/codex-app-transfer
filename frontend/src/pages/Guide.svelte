<script lang="ts">
  import { activeTab } from '../lib/store';
  import { t } from '../lib/i18n';

  function handleGoAdd() {
    activeTab.set('providers/add');
    window.location.hash = 'providers/add';
  }

  function handleGoProviders() {
    activeTab.set('providers');
    window.location.hash = 'providers';
  }
</script>

<div class="guide-page">
  <div class="page-title">
    <h1>{$t('guide.title')}</h1>
    <p class="subtitle-text">{$t('guide.subtitle')}</p>
  </div>

  <div class="guide-content">
    <!-- Prereq Card -->
    <article class="mac-card prereq-card">
      <h3><i class="bi bi-info-circle-fill"></i> {$t('guide.prereqTitle')}</h3>
      <p>{@html $t('guide.prereqBody')}</p>
    </article>

    <!-- Quick Start Steps -->
    <section class="steps-section">
      <h2>{$t('guide.quickStartTitle')}</h2>
      
      <div class="steps-grid">
        <!-- Step 1 -->
        <article class="mac-card step-card">
          <div class="step-num">1</div>
          <h4>{$t('guide.step1')}</h4>
          <p>{$t('guide.step1Text')}</p>
          <button class="link-action" on:click={handleGoAdd}>{$t('guide.step1Link')}</button>
        </article>

        <!-- Step 2 -->
        <article class="mac-card step-card">
          <div class="step-num">2</div>
          <h4>{$t('guide.step2')}</h4>
          <p>{$t('guide.step2Text')}</p>
          <button class="link-action" on:click={handleGoProviders}>{$t('guide.step2Link')}</button>
        </article>

        <!-- Step 3 -->
        <article class="mac-card step-card">
          <div class="step-num">3</div>
          <h4>{$t('guide.step3')}</h4>
          <p>{$t('guide.step3Text')}</p>
        </article>

        <!-- Step 4 -->
        <article class="mac-card step-card">
          <div class="step-num">4</div>
          <h4>{$t('guide.step4')}</h4>
          <p>{@html $t('guide.step4Text')}</p>
        </article>

        <!-- Step 5 -->
        <article class="mac-card step-card">
          <div class="step-num">5</div>
          <h4>{$t('guide.step5')}</h4>
          <p>{@html $t('guide.step5Text')}</p>
        </article>
      </div>
    </section>

    <!-- Advanced Usage -->
    <section class="advanced-section">
      <h2>{$t('guide.advancedTitle')}</h2>
      
      <div class="topics-grid">
        <article class="mac-card topic-card">
          <h4><i class="bi bi-lightbulb"></i> {$t('guide.advThinkingTitle')}</h4>
          <p>{$t('guide.advThinkingText')}</p>
        </article>

        <article class="mac-card topic-card">
          <h4><i class="bi bi-patch-check"></i> {$t('guide.advCompatTitle')}</h4>
          <p>{$t('guide.advCompatText')}</p>
        </article>

        <article class="mac-card topic-card">
          <h4><i class="bi bi-shield-check"></i> {$t('guide.advBackupTitle')}</h4>
          <p>{$t('guide.advBackupText')}</p>
        </article>

        <article class="mac-card topic-card">
          <h4><i class="bi bi-arrow-counterclockwise"></i> {$t('guide.advRestoreTitle')}</h4>
          <p>{$t('guide.advRestoreText')}</p>
        </article>
      </div>
    </section>

    <!-- Troubleshooting -->
    <section class="troubleshooting-section">
      <h2>{$t('guide.troubleshootTitle')}</h2>
      
      <div class="topics-grid">
        <article class="mac-card topic-card">
          <h4><i class="bi bi-chat-left-dots"></i> {$t('guide.tsFeedbackTitle')}</h4>
          <p>{$t('guide.tsFeedbackText')}</p>
        </article>

        <article class="mac-card topic-card">
          <h4><i class="bi bi-terminal"></i> {$t('guide.tsLogsTitle')}</h4>
          <p>{$t('guide.tsLogsText')}</p>
        </article>

        <article class="mac-card topic-card">
          <h4><i class="bi bi-arrow-repeat"></i> {$t('guide.tsRestartTitle')}</h4>
          <p>{@html $t('guide.tsRestartText')}</p>
        </article>

        <article class="mac-card topic-card">
          <h4><i class="bi bi-exclamation-triangle"></i> {$t('guide.tsSpeedTitle')}</h4>
          <p>{@html $t('guide.tsSpeedText')}</p>
        </article>
      </div>
    </section>
  </div>
</div>

<style>
  .guide-page {
    display: flex;
    flex-direction: column;
    gap: 16px;
    max-width: 900px;
    margin: 0 auto;
  }

  .guide-content {
    display: flex;
    flex-direction: column;
    gap: 24px;
  }

  .prereq-card {
    background-color: var(--mac-accent-soft);
    border-color: var(--mac-accent);
    padding: 16px 20px;
  }

  .prereq-card h3 {
    font-size: 14px;
    color: var(--mac-accent);
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 8px;
  }

  .prereq-card p {
    font-size: 13px;
    line-height: 1.6;
  }

  section h2 {
    font-size: 15px;
    font-weight: 700;
    margin-bottom: 12px;
    color: var(--mac-text-secondary);
    text-transform: uppercase;
    letter-spacing: 0.5px;
    border-bottom: 1px solid var(--mac-border-separator);
    padding-bottom: 6px;
  }

  /* Steps style */
  .steps-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(250px, 1fr));
    gap: 16px;
  }

  .step-card {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    padding: 16px;
    position: relative;
  }

  .step-num {
    font-size: 10px;
    font-weight: 700;
    color: #ffffff;
    background-color: var(--mac-accent);
    width: 20px;
    height: 20px;
    border-radius: 50%;
    display: flex;
    align-items: center;
    justify-content: center;
    margin-bottom: 12px;
  }

  .step-card h4 {
    font-size: 13px;
    font-weight: 600;
    margin-bottom: 6px;
  }

  .step-card p {
    font-size: 12px;
    color: var(--mac-text-secondary);
    line-height: 1.5;
    flex: 1;
  }

  .link-action {
    background: transparent;
    border: none;
    font-size: 11px;
    font-weight: 600;
    color: var(--mac-accent);
    cursor: pointer;
    margin-top: 12px;
    padding: 0;
    outline: none;
  }

  .link-action:hover {
    text-decoration: underline;
  }

  /* Advanced & troubleshooting topic lists */
  .topics-grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 16px;
  }

  .topic-card {
    padding: 16px;
  }

  .topic-card h4 {
    font-size: 13px;
    font-weight: 600;
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 6px;
    color: var(--mac-text-primary);
  }

  .topic-card h4 i {
    color: var(--mac-accent);
  }

  .topic-card p {
    font-size: 12px;
    color: var(--mac-text-secondary);
    line-height: 1.6;
  }
</style>
