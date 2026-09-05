import SwiftUI

struct RootView: View {
    @Environment(MailboxStore.self) private var store

    var body: some View {
        Group {
            if !store.loaded {
                ProgressView("opening mailbox")
            } else if !store.hasAccounts {
                SetupView()
            } else {
                InboxView()
            }
        }
        .alert("Error", isPresented: Binding(
            get: { store.lastError != nil },
            set: { if !$0 { store.lastError = nil } }
        )) {
            Button("OK", role: .cancel) { store.lastError = nil }
        } message: {
            Text(store.lastError ?? "")
        }
    }
}
