import GravitonKit
import SwiftUI
import UserNotifications

/// Push alerts: asks for permission, gets this phone's APNs token, and gives
/// it to every paired Mac, which then alerts on failed containers and
/// finished builds. Tapping an alert opens Activity.
@MainActor
@Observable
final class PushManager: NSObject {
    static let shared = PushManager()

    /// This device's APNs token, as hex.
    private(set) var token: String?
    /// What each Mac said when the token was registered (Mac name → status).
    private(set) var status: [String: String] = [:]
    /// Set when an alert is tapped; the app switches to Activity.
    var openActivity = false

    /// Debug builds get their alerts through APNs's sandbox.
    private var sandbox: Bool {
        #if DEBUG
        true
        #else
        false
        #endif
    }

    /// Ask once; does nothing more if the person said no.
    func start() {
        UNUserNotificationCenter.current().delegate = self
        Task {
            let granted = (try? await UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound, .badge])) ?? false
            if granted { UIApplication.shared.registerForRemoteNotifications() }
        }
    }

    func received(token data: Data) {
        token = data.map { String(format: "%02x", $0) }.joined()
        register(with: HostStore())
    }

    /// Give the token to every paired Mac (after pairing a new one, too).
    func register(with hosts: HostStore) {
        guard let token else { return }
        for host in hosts.hosts {
            guard let engine = hosts.engine(for: host.id) else { continue }
            let name = host.name
            Task {
                do {
                    let info = try await engine.registerPush(token: token, sandbox: sandbox)
                    status[name] = info.configured ? "Alerts on" : "Registered; alerts aren't set up on that Mac yet (grav push setup)"
                } catch {
                    status[name] = error.localizedDescription
                }
            }
        }
    }
}

extension PushManager: @preconcurrency UNUserNotificationCenterDelegate {
    /// Show alerts while the app is open too.
    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification) async -> UNNotificationPresentationOptions {
        [.banner, .sound, .list]
    }

    func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse) async {
        openActivity = true
    }
}

/// Hands the APNs token to `PushManager`.
final class AppDelegate: NSObject, UIApplicationDelegate {
    func application(_ application: UIApplication, didRegisterForRemoteNotificationsWithDeviceToken deviceToken: Data) {
        Task { @MainActor in PushManager.shared.received(token: deviceToken) }
    }

    func application(_ application: UIApplication, didFailToRegisterForRemoteNotificationsWithError error: Error) {
        print("push registration failed: \(error.localizedDescription)")
    }
}
