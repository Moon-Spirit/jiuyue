/**
 * Minimal stand-in for the browser `WebSocket`.
 *
 * Tests substitute this at the network boundary (`vi.stubGlobal("WebSocket",
 * FakeWebSocket)`) instead of mocking the code under test, so the client and
 * store are exercised through the same interface the browser provides.
 */
export class FakeWebSocket {
  static readonly instances: FakeWebSocket[] = [];

  static reset(): void {
    FakeWebSocket.instances.length = 0;
  }

  static latest(): FakeWebSocket {
    const socket = FakeWebSocket.instances[FakeWebSocket.instances.length - 1];
    if (socket === undefined) {
      throw new Error("FakeWebSocket: no socket has been constructed yet");
    }
    return socket;
  }

  onopen: ((event: Event) => void) | null = null;
  onmessage: ((event: MessageEvent) => void) | null = null;
  onclose: ((event: CloseEvent) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;

  readonly url: string;
  readyState = 0;

  constructor(url: string | URL) {
    this.url = String(url);
    FakeWebSocket.instances.push(this);
  }

  send(): void {
    // Outgoing frames are not under test yet.
  }

  close(): void {
    this.readyState = 3;
    this.onclose?.(new CloseEvent("close"));
  }

  emitOpen(): void {
    this.readyState = 1;
    this.onopen?.(new Event("open"));
  }

  emitMessage(data: string): void {
    this.onmessage?.(new MessageEvent("message", { data }));
  }

  emitClose(): void {
    this.readyState = 3;
    this.onclose?.(new CloseEvent("close"));
  }
}
