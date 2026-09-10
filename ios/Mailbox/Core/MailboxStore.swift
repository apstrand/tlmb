import Foundation
import Observation

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

    func toggleRead(_ message: MessageSummary) {
        let change = message.isUnread ? "+seen" : "-seen"
        mutate([message.id]) { try MailboxClient.shared.flag(ids: $0, changes: [change]) }
    }

    func toggleFlagged(_ message: MessageSummary) {
        let change = message.isFlagged ? "-flagged" : "+flagged"
        mutate([message.id]) { try MailboxClient.shared.flag(ids: $0, changes: [change]) }
    }

    private func mutate(_ ids: [String], _ work: @escaping ([String]) throws -> Void) {
        messages.removeAll { ids.contains($0.id) }
        Task {
            do {
                try work(ids)
                try await reload()
            } catch {
                lastError = error.localizedDescription
                try? await reload()
            }
        }
    }

    func addAccount(address: String, password: String, imapHost: String?, imapPort: Int?, imapUser: String? = nil, smtpHost: String?, smtpPort: Int?) async throws {
        _ = try MailboxClient.shared.addAccount(
            address: address,
            password: password,
            imapHost: imapHost,
            imapPort: imapPort,
            imapUser: imapUser,
            smtpHost: smtpHost,
            smtpPort: smtpPort
        )
        accounts = try MailboxClient.shared.listAccounts()
        try await reload()
        await refresh()
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
