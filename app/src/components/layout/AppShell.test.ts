import { describe, expect, it } from "vitest";
import { mount } from "@vue/test-utils";
import AppShell from "./AppShell.vue";

describe("AppShell — desktop nav dock", () => {
  it("floats the desktop rail as a rounded, blurred, shadowed dock", () => {
    const wrapper = mount(AppShell);
    const dock = wrapper.find('[data-testid="shell-nav-desktop"]');
    expect(dock.exists()).toBe(true);
    const classes = dock.classes();
    expect(classes).toContain("rounded-2xl");
    expect(classes).toContain("backdrop-blur-xl");
    expect(classes).toContain("bg-white/85");
    expect(classes).toContain("dark:bg-neutral-900/70");
    expect(classes).toContain("border-black/10");
    expect(classes).toContain("dark:border-white/10");
    // The dock detaches from the edge: no full-height divider any more.
    expect(classes).not.toContain("border-r");
  });

  it("uses an explicit arbitrary shadow with real alpha", () => {
    const wrapper = mount(AppShell);
    const classes = wrapper.find('[data-testid="shell-nav-desktop"]').classes();
    // `shadow-xl shadow-black/10` resolved to a fully transparent box-shadow
    // in the built CSS (Tailwind v4 color-mix): keep an explicit rgba shadow.
    expect(classes).toContain("shadow-[0_12px_32px_rgba(0,0,0,0.18)]");
    expect(classes).toContain("dark:shadow-[0_12px_32px_rgba(0,0,0,0.6)]");
    expect(classes).not.toContain("shadow-xl");
    expect(classes).not.toContain("shadow-black/10");
  });

  it("sits the dock on a tinted gutter so it never reads white-on-white", () => {
    const wrapper = mount(AppShell);
    const dock = wrapper.find('[data-testid="shell-nav-desktop"]');
    const gutter = dock.element.parentElement;
    expect(gutter?.className).toContain("bg-neutral-200/80");
    expect(gutter?.className).toContain("dark:bg-neutral-900");
    expect(gutter?.className).toContain("p-2");
  });

  it("gives the desktop grid a wider track for the floating dock", () => {
    const wrapper = mount(AppShell);
    const dock = wrapper.find('[data-testid="shell-nav-desktop"]');
    const grid = dock.element.parentElement?.parentElement;
    expect(grid?.className).toContain("lg:grid-cols-[72px_300px_1fr]");
  });

  it("keeps the tablet and mobile nav shells unchanged", () => {
    const wrapper = mount(AppShell);
    const tablet = wrapper.find('[data-testid="shell-nav-tablet"]');
    const mobile = wrapper.find('[data-testid="shell-nav-mobile"]');
    expect(tablet.classes()).toContain("border-b");
    expect(tablet.classes()).not.toContain("backdrop-blur-xl");
    expect(mobile.classes()).toContain("border-b");
    expect(mobile.classes()).not.toContain("rounded-2xl");
    // Sessions/main columns keep their edge dividers at every breakpoint.
    expect(
      wrapper.find('[data-testid="shell-sessions-desktop"]').classes(),
    ).toContain("border-r");
  });
});
