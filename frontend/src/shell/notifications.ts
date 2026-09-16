import type { MessageView } from "../generated/MessageView";
import type { DesktopShell, ShellNotification } from "./types";

/**
 * What the notification policy needs to know about the user's attention.
 *
 * Both fields are read at the moment the Message arrives; this is a decision
 * about *now*, not a subscription.
 */
export interface NotificationContext {
  /** The Conversation the user is looking at, or `null` when none is open. */
  readonly activeConversationId: string | null;
  /** Whether the application window is focused. */
  readonly windowFocused: boolean;
}

/**
 * Focus suppression (product spec §G, story 98).
 *
 * A Message gets a toast unless the user is *already looking at it* — that is,
 * the window has focus **and** the Conversation is the open one. A hidden window
 * with that Conversation selected still notifies: the user cannot see it.
 */
export function shouldNotify(
  conversationId: string,
  context: NotificationContext,
): boolean {
  return !(
    context.windowFocused && context.activeConversationId === conversationId
  );
}

/** A Message turned into everything the policy and the shell need. */
export interface MessageNotificationInput {
  readonly conversationId: string;
  readonly title: string;
  readonly body: string;
  /** A muted Conversation never notifies (product spec story 97). */
  readonly muted: boolean;
}

/**
 * Apply the policy, then hand the survivor to the shell.
 *
 * Returns whether a notification was actually shown; every refusal (muted,
 * suppressed, permission denied) is `false` rather than an error.
 */
export async function presentMessageNotification(
  shell: DesktopShell,
  input: MessageNotificationInput,
  context: NotificationContext,
): Promise<boolean> {
  if (input.muted) return false;
  if (!shouldNotify(input.conversationId, context)) return false;

  const notification: ShellNotification = {
    conversationId: input.conversationId,
    title: input.title,
    body: input.body,
  };
  return shell.notify(notification);
}

/**
 * Everything the notifier reads from application state, injected rather than
 * imported: this module stays free of stores, so it is testable in isolation.
 */
export interface MessageNotifierDeps {
  /** The Conversation currently on screen. */
  readonly activeConversationId: () => string | null;
  /** Whether the application window has focus. */
  readonly windowFocused: () => boolean;
  /** Whether a Conversation is muted. */
  readonly isMuted: (conversationId: string) => boolean;
  /** The signed-in User's id; their own Messages must never notify. */
  readonly selfUserId: () => string | null;
  /** The notification title for a Message (sender name, or group + sender). */
  readonly titleFor: (message: MessageView) => string;
}

/**
 * Build the handler that turns an incoming Message into a native notification.
 *
 * The composition root wires it to the realtime event stream; a self-message
 * (the sender's *other* Devices receive the fan-out too) is dropped before the
 * policy runs.
 */
export function createMessageNotifier(
  shell: DesktopShell,
  deps: MessageNotifierDeps,
): (message: MessageView) => Promise<boolean> {
  return async (message: MessageView): Promise<boolean> => {
    if (message.sender_id === deps.selfUserId()) return false;

    return presentMessageNotification(
      shell,
      {
        conversationId: message.conversation_id,
        title: deps.titleFor(message),
        body: message.body,
        muted: deps.isMuted(message.conversation_id),
      },
      {
        activeConversationId: deps.activeConversationId(),
        windowFocused: deps.windowFocused(),
      },
    );
  };
}
