import SwiftUI

@main
struct MailboxApp: App {
    @State private var store = MailboxStore()

    var body: some Scene {
        WindowGroup {
            RootView()
                .environment(store)
                .task { await store.boot() }
        }
    }
}
