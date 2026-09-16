import { mount } from "@vue/test-utils";
import { describe, expect, it } from "vitest";
import type { ChatMessage } from "../../stores/chat";
import MessageList from "./MessageList.vue";

const SELF_ID = "01JABC1234567890ABCDEFGHJ1";
const CONVERSATION_ID = "01JABC1234567890ABCDEFGHJ2";

/** A stored Message, as the store hands it to the list. */
function message(seq: number): ChatMessage {
  return {
    id: `M${seq}`,
    clientMsgId: `k${seq}`,
    conversationId: CONVERSATION_ID,
    senderId: SELF_ID,
    body: `消息 ${seq}`,
    seq,
    createdAtMs: 1_700_000_000_000 + seq,
    state: "sent",
    failure: null,
  };
}

function mountList(messages: ChatMessage[], hasMore = true) {
  return mount(MessageList, {
    props: {
      messages,
      currentUserId: SELF_ID,
      loading: false,
      hasMore,
      loadingOlder: false,
    },
  });
}

/**
 * jsdom has no layout engine, so the scroll metrics are stated explicitly. The
 * component reads `scrollHeight` and writes `scrollTop`; both are driven here.
 */
function defineScroll(
  element: Element,
  scrollTop: number,
  scrollHeight: number,
): void {
  Object.defineProperty(element, "scrollHeight", {
    configurable: true,
    value: scrollHeight,
  });
  element.scrollTop = scrollTop;
}

describe("MessageList", () => {
  it("asks for older history when the user scrolls to the top", async () => {
    const wrapper = mountList([message(2), message(3)]);
    const scroller = wrapper.get('[data-test="message-list"]');
    defineScroll(scroller.element, 0, 400);

    await scroller.trigger("scroll");

    expect(wrapper.emitted("loadOlder")).toHaveLength(1);
  });

  it("does not ask when the beginning of the conversation is reached", async () => {
    const wrapper = mountList([message(1)], false);
    const scroller = wrapper.get('[data-test="message-list"]');
    defineScroll(scroller.element, 0, 200);

    await scroller.trigger("scroll");

    expect(wrapper.emitted("loadOlder")).toBeUndefined();
  });

  it("keeps the viewport anchored when an older page is prepended", async () => {
    const wrapper = mountList([message(7), message(8)]);
    const scroller = wrapper.get('[data-test="message-list"]');
    const element = scroller.element;
    defineScroll(element, 0, 400);

    // Reaching the top captures the anchor and asks the store for older history.
    await scroller.trigger("scroll");
    expect(wrapper.emitted("loadOlder")).toHaveLength(1);

    // The prepend grows the content above the viewport by 300px...
    Object.defineProperty(element, "scrollHeight", {
      configurable: true,
      value: 700,
    });
    await wrapper.setProps({
      messages: [message(4), message(5), message(6), message(7), message(8)],
    });

    // ...so the offset is advanced by exactly that delta: the Message that was at
    // the top edge stays at the top edge instead of the list jumping.
    expect(element.scrollTop).toBe(300);
  });

  it("does not anchor when an older load returns no Messages", async () => {
    const wrapper = mountList([message(5)]);
    const scroller = wrapper.get('[data-test="message-list"]');
    const element = scroller.element;
    defineScroll(element, 0, 300);

    // Reaching the top captures an anchor, but the page turns out to be empty.
    await scroller.trigger("scroll");
    await wrapper.setProps({ loadingOlder: true });
    await wrapper.setProps({ loadingOlder: false });

    // A later realtime Message must not be mistaken for a prepend and yank the
    // viewport: the stale anchor was dropped when the load finished.
    Object.defineProperty(element, "scrollHeight", {
      configurable: true,
      value: 500,
    });
    await wrapper.setProps({ messages: [message(5), message(6)] });

    expect(element.scrollTop).toBe(0);
  });
});
