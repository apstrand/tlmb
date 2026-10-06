import Foundation
import Observation
import SwiftUI

@MainActor
@Observable
final class MailboxStore {
    var messages: [MessageSummary] = []
    var accounts: [Account] = []
    var query: String = ""
    var syncing = false
    var banner: String?
    var lastError: String?
    var loaded = false

    var hasAccounts: Bool { !accounts.isEmpty }

    func boot() async {
        do {
            try MailboxClient.shared.open()
            accounts = try MailboxClient.shared.listAccounts()
            MailboxClient.shared.installKeychainPasswords()
            if hasAccounts {
                try await reload()
                await refresh(quiet: true)
            }
            loaded = true
        } catch {
            lastError = error.localizedDescription
            loaded = true
        }
    }

    func reload() async throws {
        let q = Self.liveQuery(query)
        let list = try MailboxClient.shared.list(query: q.isEmpty ? nil : q)
        messages = list.messages
        accounts = (try? MailboxClient.shared.listAccounts()) ?? accounts
    }

    func refresh(quiet: Bool = false) async {
        guard hasAccounts, !syncing else { return }
        syncing = true
        if !quiet { banner = "syncing…" }
        defer { syncing = false }
        do {
            let report = try await Task.detached(priority: .userInitiated) {
                try MailboxClient.shared.sync()
            }.value
            try await reload()
            let added = report.synced.reduce(0) { $0 + Int($1.messagesAdded) }
            banner = added == 0 ? "up to date" : "\(added) new"
            clearBannerLater()
        } catch {
            lastError = error.localizedDescription
            banner = nil
        }
    }

    func archive(_ message: MessageSummary) {
        mutate([message.id]) { try MailboxClient.shared.archive(ids: $0) }
    }

    func trash(_ message: MessageSummary) {
        mutate([message.id]) { try MailboxClient.shared.trash(ids: $0) }
    }

    func spam(_ message: MessageSummary) {
        mutate([message.id]) { try MailboxClient.shared.spam(ids: $0) }
    }

    func markRead(_ message: MessageSummary) {
        guard message.isUnread else { return }
        restyle(message, "+seen")
    }

    func toggleRead(_ message: MessageSummary) {
        restyle(message, message.isUnread ? "+seen" : "-seen")
    }

    func toggleFlagged(_ message: MessageSummary) {
        restyle(message, message.isFlagged ? "-flagged" : "+flagged")
    }

    /// Runs a swipe's action on a message.
    func perform(_ action: SwipeAction, on message: MessageSummary) {
        switch action {
        case .delete: trash(message)
        case .archive: archive(message)
        case .spam: spam(message)
        case .markRead: markRead(message)
        case .toggleRead: toggleRead(message)
        case .flag: toggleFlagged(message)
        }
    }

    /// A flag change, shown on the row where it is rather than by taking the
    /// row away: a message marked read is still in the inbox. The reload
    /// afterwards is what drops it from a list filtered to `is:unread`.
    private func restyle(_ message: MessageSummary, _ change: String) {
        if let at = messages.firstIndex(where: { $0.id == message.id }) {
            let name = String(change.dropFirst())
            messages[at].flags.removeAll { $0 == name }
            if change.hasPrefix("+") { messages[at].flags.append(name) }
        }
        write { try MailboxClient.shared.flag(ids: [message.id], changes: [change]) }
    }

    private func mutate(_ ids: [String], _ work: @escaping ([String]) throws -> Void) {
        messages.removeAll { ids.contains($0.id) }
        write { try work(ids) }
    }

    private func write(_ work: @escaping () throws -> Void) {
        Task {
            do {
                try work()
                try await reload()
            } catch {
                lastError = error.localizedDescription
                try? await reload()
            }
        }
    }

    func addAccount(
        address: String,
        password: String,
        imapHost: String?,
        imapPort: Int?,
        imapSecurity: String? = nil,
        imapUser: String? = nil,
        smtpHost: String?,
        smtpPort: Int?,
        smtpSecurity: String? = nil,
        smtpUser: String? = nil
    ) async throws {
        _ = try MailboxClient.shared.addAccount(
            address: address,
            password: password,
            imapHost: imapHost,
            imapPort: imapPort,
            imapSecurity: imapSecurity,
            imapUser: imapUser,
            smtpHost: smtpHost,
            smtpPort: smtpPort,
            smtpSecurity: smtpSecurity,
            smtpUser: smtpUser
        )
        accounts = try MailboxClient.shared.listAccounts()
        try await reload()
        await refresh()
    }

    /// Takes the accounts out of a pairing export (§8.3) and starts using them.
    ///
    /// Throws rather than reporting through `lastError`, because the screen that
    /// asked has a passphrase field to put the failure beside — a wrong
    /// passphrase is a thing to try again, not an app-level error.
    @discardableResult
    func importPairing(text: String, passphrase: String) async throws -> PairImport {
        // Argon2id on the passphrase is deliberately slow (§8.3), so it does not
        // run on the main actor — the same reason discovery does not.
        let imported = try await Task.detached(priority: .userInitiated) {
            try MailboxClient.shared.pairImport(text: text, passphrase: passphrase)
        }.value
        accounts = try MailboxClient.shared.listAccounts()
        var said = imported.summary
        if !imported.needsPassword.isEmpty {
            // An account whose secret did not travel will fail to sync, and
            // finding out then is worse than being told now.
            said += " — \(imported.needsPassword.joined(separator: ", ")) still needs a password"
        }
        if hasAccounts {
            try await reload()
            await refresh(quiet: true)
        }
        // After the sync, which sets a banner of its own: what the import did is
        // the news here, and the mail arriving is visible without being told.
        banner = said
        clearBannerLater()
        return imported
    }

    func removeAccount(id: String) async {
        do {
            try MailboxClient.shared.removeAccount(id: id)
            accounts = try MailboxClient.shared.listAccounts()
            try await reload()
        } catch {
            lastError = error.localizedDescription
        }
    }

    /// Same prefix rule the TUI uses: a trailing `*` on a bare word matches as
    /// you type, so `postm` finds `postmortem`.
    static func liveQuery(_ raw: String) -> String {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty { return "" }
        if trimmed.contains(":") || trimmed.hasSuffix("*") { return trimmed }
        if let last = trimmed.split(separator: " ").last, last.contains(":") {
            return trimmed
        }
        return trimmed + "*"
    }

    private func clearBannerLater() {
        Task {
            try? await Task.sleep(for: .seconds(2.5))
            if !syncing { banner = nil }
        }
    }
}

/// What a swipe across a message row does. The same choices, and the same
/// defaults, as the web interface: left deletes, right marks read.
enum SwipeAction: String, CaseIterable, Identifiable {
    case delete, archive, spam, markRead = "mark-read", toggleRead = "toggle-read", flag

    var id: String { rawValue }

    static let leftDefault = SwipeAction.delete
    static let rightDefault = SwipeAction.markRead

    /// The settings keys the two directions are stored under.
    static let leftKey = "swipe.left"
    static let rightKey = "swipe.right"

    var title: String {
        switch self {
        case .delete: "Delete"
        case .archive: "Archive"
        case .spam: "Spam"
        case .markRead: "Mark read"
        case .toggleRead: "Toggle read"
        case .flag: "Flag"
        }
    }

    /// The label on the row, which can say which way a toggle will go.
    func label(for message: MessageSummary) -> String {
        switch self {
        case .toggleRead: message.isUnread ? "Read" : "Unread"
        case .flag: message.isFlagged ? "Unflag" : "Flag"
        case .markRead: "Read"
        default: title
        }
    }

    func systemImage(for message: MessageSummary) -> String {
        switch self {
        case .delete: "trash"
        case .archive: "archivebox"
        case .spam: "xmark.bin"
        case .markRead: "envelope.open"
        case .toggleRead: message.isUnread ? "envelope.open" : "envelope.badge"
        case .flag: "flag"
        }
    }

    var tint: Color {
        switch self {
        case .delete: .red
        case .archive: .teal
        case .spam: .brown
        case .markRead, .toggleRead: .indigo
        case .flag: .orange
        }
    }
}
