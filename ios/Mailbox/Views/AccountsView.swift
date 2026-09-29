import SwiftUI

struct AccountsView: View {
    @Environment(\.dismiss) private var dismiss
    @Environment(MailboxStore.self) private var store
    @State private var adding = false
    @AppStorage(SwipeAction.leftKey) private var swipeLeft = SwipeAction.leftDefault
    @AppStorage(SwipeAction.rightKey) private var swipeRight = SwipeAction.rightDefault

    var body: some View {
        NavigationStack {
            List {
                ForEach(store.accounts) { account in
                    VStack(alignment: .leading, spacing: 4) {
                        Text(account.address)
                            .font(.body.weight(.medium))
                        Text("\(account.imapHost):\(account.imapPort)")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                        if !account.hasCredential {
                            Text("no password in Keychain")
                                .font(.caption)
                                .foregroundStyle(.orange)
                        }
                    }
                    .swipeActions {
                        Button(role: .destructive) {
                            Task { await store.removeAccount(id: account.id) }
                        } label: {
                            Label("Remove", systemImage: "trash")
                        }
                    }
                }
                Button {
                    adding = true
                } label: {
                    Label("Add account", systemImage: "plus")
                }
                Section("Swipe") {
                    Picker("Swipe left", selection: $swipeLeft) {
                        ForEach(SwipeAction.allCases) { Text($0.title).tag($0) }
                    }
                    Picker("Swipe right", selection: $swipeRight) {
                        ForEach(SwipeAction.allCases) { Text($0.title).tag($0) }
                    }
                }
            }
            .navigationTitle("Accounts")
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }
                }
            }
            .sheet(isPresented: $adding) {
                SetupView()
            }
        }
    }
}
