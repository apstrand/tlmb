import SwiftUI

struct InboxView: View {
    @Environment(MailboxStore.self) private var store
    @State private var composing = false
    @State private var showAccounts = false
    @AppStorage(SwipeAction.leftKey) private var swipeLeft = SwipeAction.leftDefault
    @AppStorage(SwipeAction.rightKey) private var swipeRight = SwipeAction.rightDefault

    private func swipeButton(_ action: SwipeAction, _ message: MessageSummary) -> some View {
        Button(role: action == .delete ? .destructive : nil) {
            store.perform(action, on: message)
        } label: {
            Label(action.label(for: message), systemImage: action.systemImage(for: message))
        }
        .tint(action.tint)
    }

    var body: some View {
        @Bindable var store = store
        NavigationStack {
            List {
                ForEach(store.messages) { message in
                    NavigationLink(value: message) {
                        MessageRow(message: message, showAccount: store.accounts.count > 1)
                    }
                    // Swiping left reveals the trailing edge, right the
                    // leading one. The chosen action comes first, so a full
                    // swipe does it; one more sits behind it for a short
                    // swipe, unless it would repeat the first.
                    .swipeActions(edge: .trailing, allowsFullSwipe: true) {
                        swipeButton(swipeLeft, message)
                        if swipeLeft != .archive { swipeButton(.archive, message) }
                    }
                    .swipeActions(edge: .leading, allowsFullSwipe: true) {
                        swipeButton(swipeRight, message)
                        if swipeRight != .flag { swipeButton(.flag, message) }
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
