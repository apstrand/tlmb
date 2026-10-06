import Foundation
#if canImport(CMailbox)
import CMailbox
#endif

/// Thin Swift shell over the Rust core (§4). Every operation is a mailbox-api
/// JSON round-trip; the types in Types.swift are that contract.
final class MailboxClient: @unchecked Sendable {
    static let shared = MailboxClient()

    private var handle: OpaquePointer?
    private let lock = NSRecursiveLock()
    private let decoder: JSONDecoder = {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return d
    }()

    var configPath: URL { roots.config }
    var stateDir: URL { roots.state }

    private var roots: (config: URL, state: URL) {
        let appSupport = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSTemporaryDirectory())
        let root = appSupport.appendingPathComponent("mailbox", isDirectory: true)
        return (
            config: root.appendingPathComponent("config.toml"),
            state: root.appendingPathComponent("state", isDirectory: true)
        )
    }

    func open() throws {
        lock.lock()
        defer { lock.unlock() }
        if handle != nil { return }
        let r = roots
        try FileManager.default.createDirectory(at: r.state, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: r.config.deletingLastPathComponent(), withIntermediateDirectories: true)

        var error: UnsafeMutablePointer<CChar>?
        let opened = r.config.path.withCString { config in
            r.state.path.withCString { state in
                mailbox_open(config, state, &error)
            }
        }
        if let error {
            let message = String(cString: error)
            mailbox_string_free(error)
            throw MailboxError(error: .init(code: "store", message: message, retryable: false, hint: nil))
        }
        guard let opened else {
            throw MailboxError(error: .init(code: "store", message: "failed to open mailbox", retryable: false, hint: nil))
        }
        handle = opened
        installKeychainPasswords()
    }

    deinit {
        if let handle {
            mailbox_close(handle)
        }
    }

    func installKeychainPasswords() {
        guard let accounts = try? listAccounts() else { return }
        for account in accounts {
            if let password = Keychain.password(account: account.id) {
                _ = try? execRaw("set_password", ["account": account.id, "password": password])
            }
        }
    }

    func list(query: String? = nil, limit: Int = 80, cursor: String? = nil) throws -> ListReply {
        var args: [String: Any] = ["limit": limit]
        if let query, !query.isEmpty { args["query"] = query }
        if let cursor { args["cursor"] = cursor }
        return try decode("ls", args)
    }

    func search(_ query: String, limit: Int = 80) throws -> ListReply {
        try decode("search", ["query": query, "limit": limit])
    }

    func show(id: String, thread: Bool = false, format: String = "text") throws -> ShowReply {
        try decode("show", ["id": id, "thread": thread, "format": format])
    }

    func flag(ids: [String], changes: [String]) throws {
        let _: Envelope = try decode("flag", ["ids": ids, "changes": changes])
    }

    func archive(ids: [String]) throws {
        let _: Envelope = try decode("archive", ["ids": ids])
    }

    func trash(ids: [String]) throws {
        let _: Envelope = try decode("rm", ["ids": ids])
    }

    func spam(ids: [String]) throws {
        let _: Envelope = try decode("spam", ["ids": ids])
    }

    func sync(full: Bool = false) throws -> SyncReply {
        try decode("sync", ["full": full])
    }

    func ensureBody(id: String) throws {
        let _: Envelope = try decode("ensure_body", ["id": id])
    }

    func listAccounts() throws -> [Account] {
        (try decode("accounts", [:]) as AccountsReply).accounts
    }

    func folders() throws -> [Folder] {
        (try decode("folders", [:]) as FoldersReply).folders
    }

    func send(to: [String], subject: String, body: String, cc: [String] = [], from: String? = nil, inReplyTo: String? = nil, references: [String] = []) throws {
        var args: [String: Any] = [
            "to": to,
            "cc": cc,
            "subject": subject,
            "body": body,
        ]
        if let from { args["from"] = from }
        if let inReplyTo { args["in_reply_to"] = inReplyTo }
        if !references.isEmpty { args["references"] = references }
        let _: Envelope = try decode("send", args)
    }

    func replyDraft(id: String, all: Bool = false, forward: Bool = false) throws -> DraftReply {
        try decode("reply_draft", ["id": id, "all": all, "forward": forward])
    }

    func addAccount(address: String, password: String, imapHost: String? = nil, imapPort: Int? = nil, imapSecurity: String? = nil, imapUser: String? = nil, smtpHost: String? = nil, smtpPort: Int? = nil, smtpSecurity: String? = nil, smtpUser: String? = nil, requireTLS: Bool = true) throws -> String {
        var args: [String: Any] = [
            "address": address,
            "password": password,
            "require_tls": requireTLS,
        ]
        if let imapHost, !imapHost.isEmpty { args["imap_host"] = imapHost }
        if let imapPort { args["imap_port"] = imapPort }
        if let imapSecurity, !imapSecurity.isEmpty { args["imap_security"] = imapSecurity }
        if let imapUser, !imapUser.isEmpty { args["imap_user"] = imapUser }
        if let smtpHost, !smtpHost.isEmpty { args["smtp_host"] = smtpHost }
        if let smtpPort { args["smtp_port"] = smtpPort }
        if let smtpSecurity, !smtpSecurity.isEmpty { args["smtp_security"] = smtpSecurity }
        if let smtpUser, !smtpUser.isEmpty { args["smtp_user"] = smtpUser }
        struct Added: Decodable { var id: String }
        let added: Added = try decode("add_account", args)
        try Keychain.setPassword(password, account: added.id)
        return added.id
    }

    func removeAccount(id: String) throws {
        let _: Envelope = try decode("remove_account", ["id": id])
        Keychain.deletePassword(account: id)
    }

    /// The built-in table only. Cheap enough to call as the address is typed.
    func providers(address: String) throws -> ProviderHints {
        try decode("providers", ["address": address])
    }

    /// Asks the network where a domain's servers are. Blocks on DNS, so this
    /// belongs on a worker; `providers` is the instant, offline sibling. Unlike
    /// `providers` this runs §9's full chain and returns what it found along
    /// with the trail, not the flat table shape.
    func discover(address: String) throws -> Discovery {
        try decode("discover", ["address": address])
    }

    private struct Envelope: Decodable {}

    private func decode<T: Decodable>(_ method: String, _ args: [String: Any]) throws -> T {
        let data = try execRaw(method, args)
        if let err = try? decoder.decode(MailboxError.self, from: data), !err.error.message.isEmpty, err.error.code != "" {
            // A successful Reply never has a top-level `error` object.
            if (try? JSONSerialization.jsonObject(with: data) as? [String: Any])?["error"] != nil {
                throw err
            }
        }
        return try decoder.decode(T.self, from: data)
    }

    @discardableResult
    private func execRaw(_ method: String, _ args: [String: Any]) throws -> Data {
        lock.lock()
        defer { lock.unlock() }
        if handle == nil {
            try open()
        }
        guard let handle else {
            throw MailboxError(error: .init(code: "store", message: "mailbox is not open", retryable: false, hint: nil))
        }
        let json = args.isEmpty ? "{}" : String(data: try JSONSerialization.data(withJSONObject: args), encoding: .utf8) ?? "{}"
        let result = method.withCString { m in
            json.withCString { a in
                mailbox_exec(handle, m, a)
            }
        }
        guard let result else {
            throw MailboxError(error: .init(code: "failed", message: "no response", retryable: false, hint: nil))
        }
        let text = String(cString: result)
        mailbox_string_free(result)
        return Data(text.utf8)
    }
}
