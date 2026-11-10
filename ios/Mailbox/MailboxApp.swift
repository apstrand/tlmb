import SwiftUI

@main
struct MailboxApp: App {
    @State private var store = MailboxStore()
    @Environment(\.scenePhase) private var phase

    var body: some Scene {
        WindowGroup {
            RootView()
                .environment(store)
                .task {
                    await store.boot()
                    await store.syncWhileOpen()
                }
        }
        // Back from the background is when someone looks for new mail, so
        // the pass starts then rather than at the next tick.
        .onChange(of: phase) { _, now in
            if now == .active {
                Task { await store.refresh(quiet: true) }
            }
        }
    }
}
