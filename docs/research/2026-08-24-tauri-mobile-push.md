<!-- project: jiuyue -->
<!-- extracted from librarian transcript tool_0302aa6d9001zm8EhzLZm95bOu, 2026-08-24 -->
# JiuYue · Tauri 2 Mobile Maturity & Offline Push — Research Findings (2026-08)

## TL;DR Verdict

| Question | Answer |
|---|---|
| Can a Tauri app keep a WebSocket alive in background on iOS/Android? | **No — confirmed.** iOS suspends the process and kills sockets; Android Doze/OEM killers do the same. Push notifications are the only reliable offline channel. |
| Is official `tauri-plugin-notification` remote-push capable? | **No — local notifications only** (verified in source: zero FCM/APNs dependencies). |
| Does remote push for Tauri exist? | **Yes, community-grade only** (best: 78★ plugin, active Aug 2026). No official solution. |
| China vendor channels from Tauri? | **Zero existing plugins.** Requires custom Kotlin native work wrapping JPush/Getui SDKs + per-vendor store listings. |
| M1 recommendation | **Desktop-first ship; mobile = scaffold + push spike behind a flag.** Full mobile GA deferred. |

---

## 1. Tauri 2 Mobile Status (as of 2026-08)

**What's solid:**
- Official v2 docs treat iOS/Android as first-class targets; `tauri ios/android init/dev/build` generate real Xcode/Gradle projects you can extend ([docs](https://v2.tauri.app/plugin/notification/) last updated Jun 15, 2026).
- **Deep links are officially solved**: `tauri-plugin-deep-link` supports Android App Links (`assetlinks.json`) + iOS Universal Links (`apple-app-site-association`) with config in `tauri.conf.json`.

**Evidence** ([deep-link README @ db9c599](https://github.com/tauri-apps/plugins-workspace/blob/db9c5998feff9384f9cbbefcbe0d45937c00a1fc/plugins/deep-link/README.md#L5-L11)):
```
| Platform | Supported |
| Android  | ✓         |
| iOS      | ✓         |
```

**What's weak for a chat app:**
- The official plugin list contains **no background-execution, no push-registration, no foreground-service plugin** ([plugins-workspace tree](https://github.com/tauri-apps/plugins-workspace/tree/db9c5998feff9384f9cbbefcbe0d45937c00a1fc/plugins) — verified by cloning; full list is autostart…window-state).
- Foreground services *are* being used by the community but hit real bugs:
  - [tauri#15671](https://github.com/tauri-apps/tauri/issues/15671) (**open**): blank webview after task removal while a foreground service keeps the process alive
  - [tauri#11609](https://github.com/tauri-apps/tauri/issues/11609) (closed): MainActivity leaked when a foreground service runs
  - [tauri#5250](https://github.com/tauri-apps/tauri/issues/5250) (**open**): no way to disable WebView background throttling
- Independent 2026 reviews converge on "functional but younger than desktop": *"Mobile support (v2) still maturing with fewer production examples"* ([MakerStack, Mar 2026](https://makerstack.co/reviews/tauri-review/)); *"For apps that push native platform capabilities heavily, React Native or Flutter still have more mature ecosystems"* ([CoderCops, May 2026](https://blog.codercops.com/blog/tauri-2-desktop-mobile-apps-rust-web-2026)).
- **Android startup risk**: WebView init is heavy on the main thread per Google's own guidance ([Optimize WebView startup](https://developer.android.com/develop/ui/views/layout/webapps/optimize-webview-startup)), and an ecosystem-level regression in System WebView 146 (Mar–Apr 2026) spiked ANR/cold-start across all WebView-based apps ([chromium#499891885](https://issues.chromium.org/issues/499891885)). Budget splash-screen + cold-start work on low-end China-market devices.

## 2. Background Execution Reality — Confirmed Impossible as Primary Channel

**iOS**: An Apple DTS engineer states it directly — apps are *"usually suspend[ed] shortly after moving to the background"*, connections die, and *"WebSocket tasks are not supported in background sessions"* ([Apple Developer Forums #716118](https://developer.apple.com/forums/thread/716118)). WKWebView additionally **stops all JS execution** when backgrounded ([WebKit behavior, CB-10657](https://issues.apache.org/jira/browse/CB-10657)), and hybrid apps have hit permanently-broken WS states after background restore ([WebKit bug 228296 — Basecamp](https://bugs.webkit.org/show_bug.cgi?id=228296)). Since your Rust core runs in-process with the suspended app, BGTaskScheduler's opportunistic windows are the only execution you get — never assume socket liveness.

**Android**: Doze/App Standby cut network; Chinese OEM ROMs (MIUI/EMUI/ColorOS) kill background processes aggressively. A foreground service can keep the Rust+WS alive but costs a persistent notification, triggers the open Tauri bugs above, and still dies to force-stops/OEM killers.

**Conclusion**: architecture must be **foreground WebSocket + offline push**, not "keep the socket alive."

## 3. Push Notification Landscape

### 3.1 Official plugin = local only

The [official notification plugin docs](https://v2.tauri.app/plugin/notification/) expose permission/send/channels/actions APIs — **no token registration, no remote-push receive path**. Source confirms it:

**Evidence** ([notification android/build.gradle.kts @ db9c599](https://github.com/tauri-apps/plugins-workspace/blob/db9c5998feff9384f9cbbefcbe0d45937c00a1fc/plugins/notification/android/build.gradle.kts#L26-L36)):
```kotlin
dependencies {
    implementation("androidx.core:core-ktx:1.9.0")
    implementation("androidx.appcompat:appcompat:1.6.0")
    implementation("com.google.android.material:material:1.7.0")
    implementation("com.fasterxml.jackson.core:jackson-databind:2.15.3")
    // ...no com.google.firebase:*, no APNs anything
}
```

### 3.2 Community remote-push options (international market)

| Plugin | Stars / Activity | Scope | Notes |
|---|---|---|---|
| [Choochmeque/tauri-plugin-notifications](https://github.com/Choochmeque/tauri-plugin-notifications) | **78★**, pushed 2026-08-18, MIT, v0.4 | FCM (Android) + APNs (iOS/macOS) + UnifiedPush (Linux), opt-in `push-notifications` feature | Most complete; `registerForPushNotifications()` returns platform token; needs `google-services.json` in `gen/android/app/` + Google Services Gradle plugin |
| [srod/tauri-plugin-fcm](https://github.com/srod/tauri-plugin-fcm) | 3★, pushed 2026-08-21, npm v0.2.0 (11 releases since late 2025) | FCM tokens on both platforms; iOS does APNs→FCM exchange | Single-token model across platforms |
| [yanqianglu/tauri-plugin-mobile-push](https://github.com/yanqianglu/tauri-plugin-mobile-push) | 10★, pushed 2026-04-18 | APNs + FCM | Avoids ObjC swizzling via `@_cdecl` FFI + `class_addMethod`; explicit tap/deep-link events |
| [rmb707/tauri-plugin-push-notifications](https://github.com/rmb707/tauri-plugin-push-notifications) | 0★, Apr 2026 | Token acquisition only | Minimal |

Proof this pattern works in production: the Alook project ships its own in-repo `tauri-plugin-push` with a standard `FirebaseMessagingService` ([PushMessagingService.kt](https://github.com/alookai/alook/blob/main/src/desktop/src-tauri/plugins/tauri-plugin-push/android/src/main/kotlin/com/alook/push/PushMessagingService.kt#L1-L10)).

**Evidence** that these wrap real FCM ([Choochmeque @ 6dabdb4](https://github.com/Choochmeque/tauri-plugin-notifications/blob/6dabdb453c5114de5b6ba62fbb0e9916553e3dfb/android/src/main/java/app/tauri/notification/TauriFirebaseMessagingService.kt#L1-L8)):
```kotlin
package app.tauri.notification
import com.google.firebase.messaging.FirebaseMessagingService
import com.google.firebase.messaging.RemoteMessage

class TauriFirebaseMessagingService : FirebaseMessagingService() {
  override fun onNewToken(token: String) { ... }
```

⚠️ All are young (<1 year, ≤78★). Plan to **fork/vendor whichever you pick**.

### 3.3 China Android market — the hard part

- **FCM is dead on mainland China devices** (no Google Play Services). You need vendor channels: Huawei HMS / Xiaomi / OPPO / vivo / Honor (+ Meizu).
- **No Tauri/Rust integration exists** — GitHub searches for tauri+jpush/getui return nothing. Aggregator client SDKs (JPush, Getui) are native-only (Android/iOS/HarmonyOS); their server side is plain REST.
  - Getui: 离线推送 requires 厂商渠道 SDK 集成； server pushes via [REST API V2](https://docs.getui.com/getui/start/accessGuide/) (HTTP — callable from any backend language)
  - JPush: same model, aggregates 极光通道+FCM+华为+小米+OPPO+vivo+魅族+荣耀+APNs ([厂商通道集成指南](https://docs.jiguang.cn/jpush/client/Android/android_3rd_guide))
- **Business constraints that shape your roadmap** ([JPush 参数指南](https://docs.jiguang.cn/jpush/client/Android/android_3rd_param), [Getui 接入指南](https://docs.getui.com/getui/start/accessGuide/)):
  - Xiaomi/vivo/OPPO channels **require app-store listing + enterprise developer accounts** — impossible before you've cleared store review
  - Marketing-class messages capped at ~**2/day/device** on most vendors unless you apply for special message categories
  - Apps not installed from OPPO/vivo stores get no 公信消息 (public-trust) service
- **iOS is unaffected**: APNs works identically in China.

## 4. Recommended Push Architecture

```
┌─────────────────────────── Server (Rust) ───────────────────────────┐
│  trait PushChannel { async fn send(device, payload) -> Result }     │
│   ├─ ApnsChannel      (HTTP/2, .p8 key)        → all iOS users      │
│   ├─ FcmChannel       (HTTP v1 API)            → intl Android       │
│   └─ CnAggregatorChannel (JPush or Getui REST V2) → CN Android      │
│        └─ aggregator fans out to HW/XM/OPPO/vivo/Honor system bars  │
│  Router: device registry keyed by (user, platform, region, token)   │
└──────────────────────────────────────────────────────────────────────┘
                    │ HTTPS (all three are plain HTTP APIs)
┌─────────────────────────── Clients ────────────────────────────────┐
│ Desktop (Win/mac/Linux): WS only — no push needed                  │
│ iOS:  community push plugin (token reg + fg presentation)          │
│       + deep-link plugin for tap routing                           │
│       + [custom] Notification Service Extension in gen/apple Xcode │
│ Android-intl: same plugin + google-services.json in gen/android    │
│ Android-CN:   ★ CUSTOM Kotlin plugin wrapping JPush/Getui SDK      │
│               inside gen/android Gradle project ← the only         │
│               component with NO existing library                   │
└─────────────────────────────────────────────────────────────────────┘
```

**Custom native code required:** (a) China-Android vendor-channel plugin (Kotlin, ~the size of Choochmeque's Android module), (b) iOS NSE for rich/muted push if wanted (Xcode target added to generated project), (c) optional Android foreground-service plugin for "online" presence — accept it will be killed by OEMs. Everything else is existing plugins + server-side HTTP adapters.

## 5. M1 Scope Recommendation

**Ship desktop-first; mobile enters M1 as scaffold, not GA.**

1. **M1 desktop**: full feature set (WS realtime works fine in foreground on desktop).
2. **M1 mobile scaffold**: `ios/android init`, shared Vue UI running, deep-link plugin wired, push spike using Choochmeque plugin behind a feature flag — validates token flow end-to-end early.
3. **Design the `PushChannel` trait now** (it's pure server-side, zero mobile dependency) so the China aggregator adapter slots in later without rework.
4. **Defer to M2/M3**: China-Android custom vendor plugin (blocked anyway by store-listing requirements for XM/OPPO/vivo channels), iOS NSE, presence-via-foreground-service.
5. Rationale: your two hard requirements — offline push and background reliability — are precisely the least-mature parts of Tauri mobile (community plugins <1yr old; open foreground-service bugs). The desktop codebase shares ~everything with the future mobile shell, so deferring costs little; shipping unreliable mobile chat push in M1 would burn the launch.

**Uncertainty notes**: I could not find published cold-start benchmarks specific to Tauri-on-Android (only the ecosystem-wide WebView regression above) — measure on a low-end device before committing to splash timing. Choochmeque's iOS silent-push (`content-available`) handling quality is unverified — test wake-and-sync behavior during the spike.
