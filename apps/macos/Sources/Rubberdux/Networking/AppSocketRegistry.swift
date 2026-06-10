import Foundation

// MARK: - AppSocketRegistry

/// Ref-counted registry for `AppSocket` instances.
///
/// Opening the same `appID` twice increments the ref-count; closing it
/// decrements it. The underlying `AppSocket` is disconnected and removed only
/// when the ref-count reaches zero. This prevents socket leaks when multiple
/// view-controllers subscribe to the same app.
///
/// The registry is not thread-safe by itself; callers must access it from a
/// single queue or actor. In the macOS app all registry calls happen on the main
/// thread (AppKit event loop).
final class AppSocketRegistry {
    private struct Record {
        let socket: AppSocket
        var refCount: Int
    }

    private var records: [String: Record] = [:]
    private let baseURL: URL
    private let session: URLSession

    init(baseURL: URL, session: URLSession = .shared) {
        self.baseURL = baseURL
        self.session = session
    }

    // MARK: - Open / close

    /// Obtain (or create) the `AppSocket` for `appID` and increment its
    /// ref-count. Callers must balance every `open` with a matching `close`.
    @discardableResult
    func open(appID: String) -> AppSocket {
        if var record = records[appID] {
            record.refCount += 1
            records[appID] = record
            return record.socket
        }
        let socket = AppSocket(appID: appID, baseURL: baseURL, session: session)
        socket.connect()
        records[appID] = Record(socket: socket, refCount: 1)
        return socket
    }

    /// Decrement the ref-count for `appID`. When the count reaches zero the
    /// socket is disconnected and the record removed.
    func close(appID: String) {
        guard var record = records[appID] else { return }
        record.refCount -= 1
        if record.refCount <= 0 {
            record.socket.disconnect()
            records.removeValue(forKey: appID)
        } else {
            records[appID] = record
        }
    }

    /// The current ref-count for `appID`, or `0` if no socket is open.
    func refCount(for appID: String) -> Int {
        records[appID]?.refCount ?? 0
    }
}
