import SwiftUI

struct InboxView: View {
    @Environment(MailboxStore.self) private var store
    @State private var composing = false
    @State private var showAccounts = false

    var body: some View {
        @Bindable var store = store
        NavigationStack {
            List {
                ForEach(store.messages) { message in
                    NavigationLink(value: message) {
                        MessageRow(message: message, showAccount: store.accounts.count > 1)
                    }
                    .swipeActions(edge: .trailing, allowsFullSwipe: true) {
                        Button(role: .destructive) { store.trash(message) } label: {
                            Label("Delete", systemImage: "trash")
                        }
                        Button { store.archive(message) } label: {
                            Label("Archive", systemImage: "archivebox")
                        }
                        .tint(.teal)
                    }
                    .swipeActions(edge: .leading) {
                        Button { store.toggleFlagged(message) } label: {
                            Label(message.isFlagged ? "Unflag" : "Flag", systemImage: "flag")
                        }
                        .tint(.orange)
                        Button { store.toggleRead(message) } label: {
                            Label(message.isUnread ? "Read" : "Unread", systemImage: message.isUnread ? "envelope.open" : "envelope.badge")
                        }
                        .tint(.indigo)
                    }
                    .listRowInsets(EdgeInsets(top: 8, leading: 12, bottom: 8, trailing: 12))
                }
            }
            .listStyle(.plain)
            .navigationTitle("Inbox")
            .navigationDestination(for: MessageSummary.self) { message in
                ThreadView(root: message)
            }
            .searchable(text: $store.query, prompt: "from:alice is:unread")
            .onChange(of: store.query) {
                Task { try? await store.reload() }
            }
            .refreshable { await store.refresh() }
            .overlay {
                if store.messages.isEmpty && !store.syncing {
                    ContentUnavailableView {
                        Label(store.query.isEmpty ? "No mail" : "Nothing matches", systemImage: "tray")
                    } description: {
                        Text(store.query.isEmpty ? "Pull to sync, or compose something." : "Try a different query. The same grammar as the CLI.")
                    }
                }
            }
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    Button { showAccounts = true } label: {
                        Image(systemName: "person.crop.circle")
                    }
                    .accessibilityLabel("Accounts")
                }
                ToolbarItem(placement: .status) {
                    if store.syncing {
                        ProgressView()
                    } else if let banner = store.banner {
                        Text(banner)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
                ToolbarItem(placement: .topBarTrailing) {
                    Button { composing = true } label: {
                        Image(systemName: "square.and.pencil")
                    }
                    .accessibilityLabel("Compose")
                }
            }
            .sheet(isPresented: $composing) {
                ComposeView()
            }
            .sheet(isPresented: $showAccounts) {
                AccountsView()
            }
        }
    }
}
