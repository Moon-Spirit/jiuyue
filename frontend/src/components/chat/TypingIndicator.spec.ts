import { mount } from "@vue/test-utils";
import { describe, expect, it } from "vitest";
import TypingIndicator from "./TypingIndicator.vue";

/** Mount the indicator with resolved display names, as the view passes them. */
function mountIndicator(names: string[]) {
  return mount(TypingIndicator, { props: { names } });
}

describe("TypingIndicator", () => {
  it("shows nothing when nobody is typing", () => {
    const wrapper = mountIndicator([]);

    expect(wrapper.find('[data-test="typing-indicator"]').exists()).toBe(false);
  });

  it("names a single Participant", () => {
    const wrapper = mountIndicator(["Alice"]);

    expect(wrapper.get('[data-test="typing-indicator"]').text()).toContain(
      "Alice 正在输入…",
    );
  });

  it("names both Participants when two are typing in a Group", () => {
    const wrapper = mountIndicator(["Alice", "Bob"]);

    expect(wrapper.get('[data-test="typing-indicator"]').text()).toContain(
      "Alice、Bob 正在输入…",
    );
  });

  it("summarises a crowd by count instead of rendering a paragraph", () => {
    const wrapper = mountIndicator(["Alice", "Bob", "Carol"]);

    expect(wrapper.get('[data-test="typing-indicator"]').text()).toContain(
      "3 人正在输入…",
    );
  });

  it("never renders a blank name as a dangling separator", () => {
    const wrapper = mountIndicator(["", "Alice"]);

    expect(wrapper.get('[data-test="typing-indicator"]').text()).toContain(
      "Alice 正在输入…",
    );
  });
});
