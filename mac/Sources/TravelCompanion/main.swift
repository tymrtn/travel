import AppKit
import EventKit
import Foundation
import Security

struct Item: Codable {
    let id, kind, title, notes, status, updated_at: String
    let start, end, url: String?
    let completed: Bool
    let alarm_minutes: Int?
}
struct Projection: Codable { let version: Int; let items: [Item] }
struct Mapping: Codable { var nativeID, revision, fingerprint: String }
struct Connection {
    let address, token: String
    let calendars, reminders: Bool
    func request(_ path: String) throws -> URLRequest {
        guard let base = URL(string: address), base.user == nil, base.password == nil,
              base.scheme == "https" || (base.scheme == "http" && ["localhost", "127.0.0.1", "::1"].contains(base.host ?? "")),
              let url = URL(string: path, relativeTo: base) else { throw URLError(.badURL) }
        var request = URLRequest(url: url); request.timeoutInterval = 20
        if !token.isEmpty { request.setValue("Bearer " + token, forHTTPHeaderField: "Authorization") }
        return request
    }
}

// Never follow a server redirect while carrying the household credential.
final class NoRedirects: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
    func urlSession(_ session: URLSession, task: URLSessionTask, willPerformHTTPRedirection response: HTTPURLResponse, newRequest request: URLRequest, completionHandler: @escaping (URLRequest?) -> Void) { completionHandler(nil) }
}

enum Secrets {
    static let query: [String: Any] = [kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: "travel-companion", kSecAttrAccount as String: "server-token"]
    static func read() -> String {
        var q = query; q[kSecReturnData as String] = true
        var result: CFTypeRef?
        guard SecItemCopyMatching(q as CFDictionary, &result) == errSecSuccess, let data = result as? Data else { return "" }
        return String(data: data, encoding: .utf8) ?? ""
    }
    static func save(_ value: String) throws {
        let data = Data(value.utf8)
        let result = SecItemUpdate(query as CFDictionary, [kSecValueData as String: data] as CFDictionary)
        if result == errSecItemNotFound {
            var q = query; q[kSecValueData as String] = data
            guard SecItemAdd(q as CFDictionary, nil) == errSecSuccess else { throw NSError(domain: "TravelKeychain", code: 1) }
        } else if result != errSecSuccess { throw NSError(domain: "TravelKeychain", code: Int(result)) }
    }
}

@MainActor final class Companion: NSObject, NSApplicationDelegate {
    let store = EKEventStore()
    var status: NSStatusItem!
    var window: NSWindow!
    let address = NSTextField(string: UserDefaults.standard.string(forKey: "server") ?? "http://127.0.0.1:3150")
    let token = NSSecureTextField(string: "")
    let message = NSTextField(wrappingLabelWithString: "Choose a server and enable Calendar or Reminders. Local destinations are required by default.")
    let calendars = NSButton(checkboxWithTitle: "Sync calendar events", target: nil, action: nil)
    let reminders = NSButton(checkboxWithTitle: "Sync reminders", target: nil, action: nil)
    var mappings: [String: Mapping] = [:]
    var syncing = false
    var configuring = false
    var timer: Timer?
    var service: Process?
    var connection: Connection?
    let network = URLSession(configuration: .ephemeral, delegate: NoRedirects(), delegateQueue: nil)

    func applicationDidFinishLaunching(_ notification: Notification) {
        token.stringValue = Secrets.read()
        connection = Connection(address: address.stringValue, token: token.stringValue, calendars: UserDefaults.standard.bool(forKey: "calendars"), reminders: UserDefaults.standard.bool(forKey: "reminders"))
        if let data = UserDefaults.standard.data(forKey: "mappings") { mappings = (try? JSONDecoder().decode([String: Mapping].self, from: data)) ?? [:] }
        status = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        status.button?.title = "Travel"
        let menu = NSMenu()
        menu.addItem(withTitle: "Settings and synchronization…", action: #selector(show), keyEquivalent: "")
        menu.addItem(withTitle: "Open Travel", action: #selector(openTravel), keyEquivalent: "")
        menu.addItem(withTitle: "Quit", action: #selector(quit), keyEquivalent: "q")
        for item in menu.items { item.target = self }
        status.menu = menu
        buildWindow()
        if let executable = Bundle.main.url(forResource: "travel", withExtension: nil), address.stringValue == "http://127.0.0.1:3150" {
            let process = Process(); process.executableURL = executable; process.arguments = ["serve"]
            process.standardOutput = FileHandle.nullDevice; process.standardError = FileHandle.nullDevice
            do { try process.run(); service = process } catch { message.stringValue = "Could not start the bundled server. Open Travel to check an existing instance." }
        }
        calendars.state = UserDefaults.standard.bool(forKey: "calendars") ? .on : .off
        reminders.state = UserDefaults.standard.bool(forKey: "reminders") ? .on : .off
        timer = Timer.scheduledTimer(withTimeInterval: 60, repeats: true) { [weak self] _ in Task { @MainActor in await self?.sync() } }
        NSWorkspace.shared.notificationCenter.addObserver(self, selector: #selector(wake), name: NSWorkspace.didWakeNotification, object: nil)
        show()
    }

    func buildWindow() {
        window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 520, height: 310), styleMask: [.titled, .closable], backing: .buffered, defer: false)
        window.title = "Travel companion"; window.isReleasedWhenClosed = false
        let save = NSButton(title: "Save and synchronize", target: self, action: #selector(enable))
        let stack = NSStackView(views: [NSTextField(labelWithString: "Travel server address"), address, NSTextField(labelWithString: "Server access token (stored in Keychain)"), token, calendars, reminders, save, message])
        stack.orientation = .vertical; stack.alignment = .leading; stack.spacing = 10
        stack.translatesAutoresizingMaskIntoConstraints = false
        window.contentView!.addSubview(stack)
        NSLayoutConstraint.activate([stack.leadingAnchor.constraint(equalTo: window.contentView!.leadingAnchor, constant: 20), stack.trailingAnchor.constraint(equalTo: window.contentView!.trailingAnchor, constant: -20), stack.topAnchor.constraint(equalTo: window.contentView!.topAnchor, constant: 20), address.widthAnchor.constraint(equalTo: stack.widthAnchor), token.widthAnchor.constraint(equalTo: stack.widthAnchor)])
    }
    @objc func show() { window.center(); window.makeKeyAndOrderFront(nil); NSApp.activate(ignoringOtherApps: true) }
    @objc func quit() { service?.terminate(); NSApp.terminate(nil) }
    @objc func openTravel() { if let configured = connection, let url = try? configured.request("/travel").url { NSWorkspace.shared.open(url) } }
    @objc func wake() { Task { await sync() } }
    @objc func enable() {
        Task {
            guard !configuring else { return }
            configuring = true
            defer { configuring = false }
            let proposed = Connection(address: address.stringValue, token: token.stringValue, calendars: calendars.state == .on, reminders: reminders.state == .on)
            guard (try? proposed.request("/api/health")) != nil else { message.stringValue = "Use HTTPS for remote servers or HTTP on localhost."; return }
            if proposed.address != connection?.address && (!mappings.isEmpty || syncing) {
                message.stringValue = "This companion is paired with another server. Export your settings before changing the pairing."; return
            }
            do {
                let calendarAllowed = proposed.calendars ? try await store.requestFullAccessToEvents() : false
                let reminderAllowed = proposed.reminders ? try await store.requestFullAccessToReminders() : false
                try Secrets.save(proposed.token)
                connection = Connection(address: proposed.address, token: proposed.token, calendars: calendarAllowed, reminders: reminderAllowed)
                UserDefaults.standard.set(proposed.address, forKey: "server")
                UserDefaults.standard.set(calendarAllowed, forKey: "calendars")
                UserDefaults.standard.set(reminderAllowed, forKey: "reminders")
                calendars.state = calendarAllowed ? .on : .off
                reminders.state = reminderAllowed ? .on : .off
                configuring = false
                await sync()
            } catch { message.stringValue = "Permission or Keychain access failed. You can continue using calendar files in Travel." }
        }
    }
    func destination(_ type: EKEntityType) throws -> EKCalendar {
        let preference = type == .event ? "calendarID" : "reminderID"
        if let id = UserDefaults.standard.string(forKey: preference), let existing = store.calendar(withIdentifier: id), existing.source.sourceType == .local, existing.allowsContentModifications { return existing }
        guard let source = store.sources.first(where: { $0.sourceType == .local }) else { throw NSError(domain: "No writable local calendar/reminder store. Use calendar files or local notifications; no iCloud destination was selected.", code: 1) }
        let calendar = EKCalendar(for: type, eventStore: store); calendar.title = "Travel"; calendar.source = source
        try store.saveCalendar(calendar, commit: true)
        UserDefaults.standard.set(calendar.calendarIdentifier, forKey: preference)
        return calendar
    }
    func date(_ value: String?) -> Date? {
        guard let value else { return nil }
        let formatter = ISO8601DateFormatter(); formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        if let date = formatter.date(from: value) { return date }
        formatter.formatOptions = [.withInternetDateTime]; return formatter.date(from: value)
    }
    func fingerprint(_ item: EKCalendarItem) -> String {
        let alarms = (item.alarms ?? []).map { "\($0.relativeOffset):\($0.absoluteDate?.timeIntervalSince1970.description ?? "")" }.sorted().joined(separator: ",")
        let base = "v2|\(item.title ?? "")|\(item.notes ?? "")|\(item.url?.absoluteString ?? "")|\(alarms)"
        if let event = item as? EKEvent { return base + "|\(event.startDate.timeIntervalSince1970)|\(event.endDate.timeIntervalSince1970)" }
        if let reminder = item as? EKReminder { return base + "|\(String(describing: reminder.dueDateComponents))" }
        return base
    }
    func sync() async {
        guard !syncing, !configuring, let configured = connection, configured.calendars || configured.reminders else { return }
        syncing = true; defer { syncing = false }
        do {
            let (data, response) = try await network.data(for: configured.request("/api/v1/native/projection"))
            guard (response as? HTTPURLResponse)?.statusCode == 200 else { throw URLError(.userAuthenticationRequired) }
            let projection = try JSONDecoder().decode(Projection.self, from: data)
            guard projection.version == 1 else { throw URLError(.cannotParseResponse) }
            var conflicts = 0
            for item in projection.items {
                if item.kind == "event" && !configured.calendars || item.kind == "reminder" && !configured.reminders { continue }
                let previous = mappings[item.id]
                let existing = previous.flatMap { store.calendarItem(withIdentifier: $0.nativeID) }
                if let existing, let previous, fingerprint(existing) != previous.fingerprint { conflicts += 1; continue }
                if item.status == "cancelled" || item.status == "canceled" {
                    if let event = existing as? EKEvent { try store.remove(event, span: .thisEvent, commit: true) }
                    if let reminder = existing as? EKReminder { try store.remove(reminder, commit: true) }
                    mappings.removeValue(forKey: item.id)
                    UserDefaults.standard.set(try JSONEncoder().encode(mappings), forKey: "mappings")
                    continue
                }
                if let reminder = existing as? EKReminder, let previous, previous.revision == item.updated_at, reminder.isCompleted != item.completed {
                    var req = try configured.request("/api/v1/tasks/\(item.id)/completion"); req.httpMethod = "PUT"
                    if configured.token.isEmpty {
                        let (csrfData, csrfResponse) = try await network.data(for: configured.request("/api/csrf"))
                        guard let headers = (csrfResponse as? HTTPURLResponse)?.allHeaderFields as? [String: String],
                              let csrfURL = csrfResponse.url,
                              let csrf = (try JSONSerialization.jsonObject(with: csrfData) as? [String: String])?["token"] else { throw URLError(.userAuthenticationRequired) }
                        let cookies = HTTPCookie.cookies(withResponseHeaderFields: headers, for: csrfURL)
                        req.setValue(HTTPCookie.requestHeaderFields(with: cookies)["Cookie"], forHTTPHeaderField: "Cookie")
                        req.setValue(csrf, forHTTPHeaderField: "X-Travel-CSRF")
                    }
                    req.setValue("application/json", forHTTPHeaderField: "Content-Type")
                    req.httpBody = try JSONSerialization.data(withJSONObject: ["completed": reminder.isCompleted, "expected_updated_at": item.updated_at])
                    let (_, response) = try await network.data(for: req)
                    if (response as? HTTPURLResponse)?.statusCode != 200 { conflicts += 1 }
                    continue
                }
                if let previous, previous.revision == item.updated_at, existing != nil { continue }
                let native: EKCalendarItem
                if item.kind == "event" {
                    guard let start = date(item.start), let end = date(item.end), end >= start else { conflicts += 1; continue }
                    let event = (existing as? EKEvent) ?? EKEvent(eventStore: store)
                    event.calendar = try destination(.event); event.startDate = start; event.endDate = end; native = event
                } else {
                    let reminder = (existing as? EKReminder) ?? EKReminder(eventStore: store)
                    reminder.calendar = try destination(.reminder); reminder.isCompleted = item.completed
                    if let due = date(item.start) { reminder.dueDateComponents = Calendar.current.dateComponents([.year,.month,.day,.hour,.minute,.timeZone], from: due) }
                    else if item.start == nil { reminder.dueDateComponents = nil }
                    else { conflicts += 1; continue }
                    native = reminder
                }
                native.title = item.title; native.notes = item.notes + "\nTravel item: " + item.id
                native.url = item.url.flatMap(URL.init(string:))
                native.alarms = item.alarm_minutes.map { [EKAlarm(relativeOffset: -Double($0 * 60))] }
                if let event = native as? EKEvent { try store.save(event, span: .thisEvent, commit: true) }
                if let reminder = native as? EKReminder { try store.save(reminder, commit: true) }
                mappings[item.id] = Mapping(nativeID: native.calendarItemIdentifier, revision: item.updated_at, fingerprint: fingerprint(native))
                UserDefaults.standard.set(try JSONEncoder().encode(mappings), forKey: "mappings")
            }
            message.stringValue = conflicts == 0 ? "Updated at \(Date().formatted())." : "\(conflicts) items need review; local edits were preserved."
        } catch { message.stringValue = "Sync unavailable: \(error.localizedDescription). Existing calendar items remain available." }
    }
}
MainActor.assumeIsolated {
    let app = NSApplication.shared
    let delegate = Companion()
    app.delegate = delegate
    app.setActivationPolicy(.accessory)
    withExtendedLifetime(delegate) { app.run() }
}
