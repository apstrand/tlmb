import SwiftUI

struct SetupView: View {
    @Environment(MailboxStore.self) private var store

    @State private var address = ""
    @State private var password = ""
    @State private var imapHost = ""
    @State private var imapPort = ""
    @State private var smtpHost = ""
    @State private var smtpPort = ""
    @State private var knownProvider = false
    @State private var showAdvanced = false
    @State private var working = false

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("you@example.com", text: $address)
                        .textInputAutocapitalization(.never)
                        .keyboardType(.emailAddress)
                        .autocorrectionDisabled()
                        .onChange(of: address) { lookupProvider() }
                    SecureField("Password", text: $password)
                } header: {
                    Text("Account")
                } footer: {
                    Text(knownProvider
                         ? "Server settings filled in from the address."
                         : "The password is stored in the iOS Keychain, never in the config file.")
                }

                if showAdvanced || !knownProvider {
                    Section("IMAP") {
                        TextField("Host", text: $imapHost)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                        TextField("Port", text: $imapPort)
                            .keyboardType(.numberPad)
                    }
                    Section("SMTP") {
                        TextField("Host", text: $smtpHost)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                        TextField("Port", text: $smtpPort)
                            .keyboardType(.numberPad)
                    }
                }

                Section {
                    Button {
                        Task { await add() }
                    } label: {
                        HStack {
                            Spacer()
                            if working { ProgressView() }
                            else { Text("Add account") }
                            Spacer()
                        }
                    }
                    .disabled(working || address.isEmpty || password.isEmpty)
                }
            }
            .navigationTitle("mailbox")
            .toolbar {
                ToolbarItem(placement: .topBarTrailing) {
                    Button(showAdvanced ? "Hide servers" : "Servers") {
                        showAdvanced.toggle()
                    }
                }
            }
        }
    }

    private func lookupProvider() {
        guard address.contains("@") else {
            knownProvider = false
            return
        }
        guard let hints = try? MailboxClient.shared.providers(address: address), hints.known else {
            knownProvider = false
            return
        }
        knownProvider = true
        imapHost = hints.imapHost ?? imapHost
        if let p = hints.imapPort { imapPort = String(p) }
        smtpHost = hints.smtpHost ?? smtpHost
        if let p = hints.smtpPort { smtpPort = String(p) }
    }

    private func add() async {
        working = true
        defer { working = false }
        do {
            try MailboxClient.shared.open()
            try await store.addAccount(
                address: address.trimmingCharacters(in: .whitespacesAndNewlines),
                password: password,
                imapHost: imapHost.isEmpty ? nil : imapHost,
                imapPort: Int(imapPort),
                smtpHost: smtpHost.isEmpty ? nil : smtpHost,
                smtpPort: Int(smtpPort)
            )
        } catch {
            store.lastError = error.localizedDescription
        }
    }
}
