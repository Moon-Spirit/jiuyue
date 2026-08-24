<script setup lang="ts">
/**
 * Responsive application shell.
 *
 * - ≥1024px (lg): [56px nav rail | 300px sessions | 1fr main]
 * - 640–1023px (sm–lg): top bar + [300px sessions | 1fr main]
 * - <640px: single column (top bar + main) + bottom tab placeholder
 *
 * Slots are re-instantiated per breakpoint; stateful children stay consistent
 * because their state lives in stores/composables, not local DOM.
 */
</script>

<template>
  <div
    class="h-dvh w-full overflow-hidden bg-white text-neutral-900 dark:bg-neutral-950 dark:text-neutral-100"
  >
    <!-- Desktop: three columns -->
    <div class="hidden h-full lg:grid lg:grid-cols-[56px_300px_1fr]">
      <aside
        class="flex flex-col items-center gap-2 border-r border-neutral-200 py-3 dark:border-neutral-800"
        data-testid="shell-nav-desktop"
      >
        <slot name="nav" />
      </aside>
      <section
        class="overflow-y-auto border-r border-neutral-200 dark:border-neutral-800"
        data-testid="shell-sessions-desktop"
      >
        <slot name="sessions" />
      </section>
      <main class="min-w-0 overflow-y-auto" data-testid="shell-main-desktop">
        <slot name="main" />
      </main>
    </div>

    <!-- Tablet: top bar + two columns -->
    <div class="hidden h-full sm:flex sm:flex-col lg:hidden">
      <header
        class="flex h-12 shrink-0 items-center gap-2 border-b border-neutral-200 px-3 dark:border-neutral-800"
        data-testid="shell-nav-tablet"
      >
        <slot name="nav" />
      </header>
      <div class="grid min-h-0 flex-1 grid-cols-[300px_1fr]">
        <section
          class="overflow-y-auto border-r border-neutral-200 dark:border-neutral-800"
          data-testid="shell-sessions-tablet"
        >
          <slot name="sessions" />
        </section>
        <main class="min-w-0 overflow-y-auto" data-testid="shell-main-tablet">
          <slot name="main" />
        </main>
      </div>
    </div>

    <!-- Mobile: single column + bottom tab placeholder -->
    <div class="flex h-full flex-col sm:hidden">
      <header
        class="flex h-12 shrink-0 items-center gap-2 border-b border-neutral-200 px-3 dark:border-neutral-800"
        data-testid="shell-nav-mobile"
      >
        <slot name="nav" />
      </header>
      <main
        class="min-h-0 flex-1 overflow-y-auto"
        data-testid="shell-main-mobile"
      >
        <slot name="main" />
      </main>
      <nav
        class="flex h-14 shrink-0 items-center justify-around border-t border-neutral-200 dark:border-neutral-800"
        aria-label="tabs"
        data-testid="shell-tabs-mobile"
      >
        <span
          class="h-6 w-6 rounded-md bg-neutral-200 dark:bg-neutral-700"
        ></span>
        <span
          class="h-6 w-6 rounded-md bg-neutral-200 dark:bg-neutral-700"
        ></span>
        <span
          class="h-6 w-6 rounded-md bg-neutral-200 dark:bg-neutral-700"
        ></span>
      </nav>
    </div>
  </div>
</template>
