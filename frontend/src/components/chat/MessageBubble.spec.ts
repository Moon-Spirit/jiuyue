import { mount } from "@vue/test-utils";
import { describe, expect, it } from "vitest";
import type { ChatMessage, DeliveryState } from "../../stores/chat";
import MessageBubble from "./MessageBubble.vue";

const SELF_ID = "01JABC1234567890ABCDEFGHJ1";
const CONVERSATION_ID = "01JABC1234567890ABCDEFGHJ2";

/** A stored Message in the given delivery state. */
function message(state: DeliveryState): ChatMessage {
  return {
    id: "M1",
    clientMsgId: "k1",
    conversationId: CONVERSATION_ID,
    senderId: SELF_ID,
    body: "消息",
    seq: 1,
    createdAtMs: 1_700_000_000_000,
    state,
    failure: null,
  };
}

describe("MessageBubble", () => {
  it("shows 已读 when the peer's public receipt has reached the Message", () => {
    const wrapper = mount(MessageBubble, {
      props: { message: message("sent"), own: true, readByPeer: true },
    });

    expect(wrapper.get('[data-test="delivery-state"]').text()).toBe("已读");
  });

  it("shows 已发送 when the peer has not read it yet", () => {
    const wrapper = mount(MessageBubble, {
      props: { message: message("sent"), own: true, readByPeer: false },
    });

    expect(wrapper.get('[data-test="delivery-state"]').text()).toBe("已发送");
  });

  it("never shows a receipt on the peer's own Message", () => {
    const wrapper = mount(MessageBubble, {
      props: { message: message("sent"), own: false, readByPeer: true },
    });

    expect(wrapper.find('[data-test="delivery-state"]').exists()).toBe(false);
  });
});
