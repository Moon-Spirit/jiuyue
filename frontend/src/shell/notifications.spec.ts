import { describe, expect, it, vi } from "vitest";
import type { MessageView } from "../generated/MessageView";
import { createFakeShell } from "./fake-shell";
import {
  createMessageNotifier,
  presentMessageNotification,
  shouldNotify,
} from "./notifications";

function message(overrides: Partial<MessageView> = {}): MessageView {
  return {
    id: "m1",
    conversation_id: "c1",
    seq: 1,
    sender_id: "peer",
    client_msg_id: "local:1",
    body: "在吗",
    created_at_ms: 1_700_000_000_000,
    ...overrides,
  };
}

describe("the notification policy", () => {
  it.each([
    {
      context: { activeConversationId: "c1", windowFocused: true },
      expected: false,
    },
    {
      context: { activeConversationId: "c1", windowFocused: false },
      expected: true,
    },
    {
      context: { activeConversationId: "c2", windowFocused: true },
      expected: true,
    },
    {
      context: { activeConversationId: null, windowFocused: true },
      expected: true,
    },
    {
      context: { activeConversationId: "c2", windowFocused: false },
      expected: true,
    },
  ])("notifies=$expected for $context", ({ context, expected }) => {
    expect(shouldNotify("c1", context)).toBe(expected);
  });

  it("suppresses a Message the user is already looking at", async () => {
    const shell = createFakeShell();

    const shown = await presentMessageNotification(
      shell,
      { conversationId: "c1", title: "小明", body: "在吗", muted: false },
      { activeConversationId: "c1", windowFocused: true },
    );

    expect(shown).toBe(false);
    expect(shell.notify).not.toHaveBeenCalled();
  });

  it("notifies for a different Conversation while the window has focus", async () => {
    const shell = createFakeShell();

    const shown = await presentMessageNotification(
      shell,
      { conversationId: "c2", title: "小明", body: "在吗", muted: false },
      { activeConversationId: "c1", windowFocused: true },
    );

    expect(shown).toBe(true);
    expect(shell.notify).toHaveBeenCalledWith({
      conversationId: "c2",
      title: "小明",
      body: "在吗",
    });
  });

  it("never notifies for a muted Conversation", async () => {
    const shell = createFakeShell();

    const shown = await presentMessageNotification(
      shell,
      { conversationId: "c2", title: "小明", body: "在吗", muted: true },
      { activeConversationId: "c1", windowFocused: false },
    );

    expect(shown).toBe(false);
    expect(shell.notify).not.toHaveBeenCalled();
  });
});

describe("the message notifier", () => {
  function notifier(
    shell = createFakeShell(),
    state = {
      activeConversationId: "c1" as string | null,
      windowFocused: true,
      muted: new Set<string>(),
      selfUserId: "me",
    },
  ) {
    const handle = createMessageNotifier(shell, {
      activeConversationId: () => state.activeConversationId,
      windowFocused: () => state.windowFocused,
      isMuted: (conversationId) => state.muted.has(conversationId),
      selfUserId: () => state.selfUserId,
      titleFor: (view) => `title:${view.conversation_id}`,
    });
    return { handle, shell, state };
  }

  it("drops the sender's own Message echoed back from another Device", async () => {
    const { handle, shell } = notifier();

    const shown = await handle(
      message({ sender_id: "me", conversation_id: "c2" }),
    );

    expect(shown).toBe(false);
    expect(shell.notify).not.toHaveBeenCalled();
  });

  it("notifies for a peer Message in an unfocused Conversation", async () => {
    const { handle, shell, state } = notifier();
    state.activeConversationId = "c1";
    state.windowFocused = false;

    const shown = await handle(message({ conversation_id: "c2" }));

    expect(shown).toBe(true);
    expect(shell.notify).toHaveBeenCalledWith({
      conversationId: "c2",
      title: "title:c2",
      body: "在吗",
    });
  });

  it("does not notify a muted Conversation", async () => {
    const shell = createFakeShell();
    const { handle, state } = notifier(shell);
    state.muted.add("c2");

    expect(await handle(message({ conversation_id: "c2" }))).toBe(false);
    expect(shell.notify).not.toHaveBeenCalled();
  });

  it("passes the Message body through unchanged", async () => {
    const shell = createFakeShell();
    const { handle, state } = notifier(shell);
    state.activeConversationId = null;

    await handle(message({ body: "语音消息请查收 🎧" }));

    expect(vi.mocked(shell.notify).mock.calls[0]?.[0].body).toBe(
      "语音消息请查收 🎧",
    );
  });
});
