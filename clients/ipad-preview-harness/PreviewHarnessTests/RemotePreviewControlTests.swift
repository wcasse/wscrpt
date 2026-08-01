import NIOCore
import XCTest
@testable import PreviewHarness

final class RemotePreviewControlTests: XCTestCase {
    func testSessionListAcceptsOnlyExactRemoteLoopback() throws {
        let sessions = try RemotePreviewSessionList.parse(
            listPayload(host: "127.0.0.1", port: 7_331)
        )

        XCTAssertEqual(
            sessions,
            [
                RemotePreviewSession(
                    id: "session-019d",
                    state: "ready",
                    remoteSignalingPort: 7_331,
                    sourceWidth: 1_280,
                    sourceHeight: 720
                ),
            ]
        )
        XCTAssertTrue(try XCTUnwrap(sessions.first).isAttachable)

        XCTAssertThrowsError(
            try RemotePreviewSessionList.parse(
                listPayload(host: "192.168.1.40", port: 7_331)
            )
        ) { error in
            XCTAssertEqual(error as? RemotePreviewControlError, .nonLoopbackRemote)
        }

        let connected = try RemotePreviewSessionList.parse(
            listPayload(
                host: "127.0.0.1",
                port: 7_331,
                state: "connected"
            )
        )
        XCTAssertTrue(try XCTUnwrap(connected.first).isAttachable)
    }

    func testSessionListRejectsUnknownFieldsAndInactiveHealth() throws {
        var object = try XCTUnwrap(
            JSONSerialization.jsonObject(
                with: listPayload(host: "127.0.0.1", port: 7_331)
            ) as? [String: Any]
        )
        object["future"] = true
        XCTAssertThrowsError(
            try RemotePreviewSessionList.parse(
                JSONSerialization.data(withJSONObject: object)
            )
        )

        let inactive = try RemotePreviewSessionList.parse(
            listPayload(
                host: "127.0.0.1",
                port: 7_331,
                healthActive: false
            )
        )
        XCTAssertFalse(try XCTUnwrap(inactive.first).isAttachable)
    }

    func testCommandBuilderQuotesWorkspaceAndSessionWithoutInterpolation() throws {
        let builder = try RemotePreviewCommandBuilder(
            workspacePath: "/srv/Developer's Game",
            previewctlRelativePath: "previewd/bin/previewctl.mjs"
        )

        let list = builder.list()
        XCTAssertTrue(list.hasPrefix("exec \"$SHELL\" -lc "))
        let expectedInner = [
            "cd -- \(POSIXShell.pathExpression("/srv/Developer's Game")) && node",
            "--",
            POSIXShell.quote("./previewd/bin/previewctl.mjs"),
            "list --workspace \(POSIXShell.quote(".")) --json",
        ].joined(separator: " ")
        XCTAssertEqual(
            list,
            "exec \"$SHELL\" -lc \(POSIXShell.quote(expectedInner))"
        )
        XCTAssertFalse(list.contains("\n"))

        XCTAssertThrowsError(
            try RemotePreviewCommandBuilder(
                workspacePath: "/srv/game",
                previewctlRelativePath: "../previewctl.mjs"
            )
        ) { error in
            XCTAssertEqual(
                error as? RemotePreviewControlError,
                .invalidCommandConfiguration
            )
        }

        let homeBuilder = try RemotePreviewCommandBuilder(
            workspacePath: "~/Developer's Game"
        )
        let homeList = homeBuilder.list()
        XCTAssertTrue(homeList.contains("\"$HOME\"/"))
        XCTAssertTrue(homeList.contains("--workspace"))
        XCTAssertFalse(homeList.contains("cd '~"))
    }

    func testCommandBuilderSupportsSeparateQuotedPreviewToolsCheckout() throws {
        let builder = try RemotePreviewCommandBuilder(
            workspacePath: "/srv/Developer's Game",
            previewToolsPath: "~/src/wscrpt tools"
        )

        let command = builder.list()
        XCTAssertTrue(command.contains("cd --"))
        XCTAssertTrue(command.contains("/srv/Developer"))
        XCTAssertTrue(command.contains("\"$HOME\"/"))
        XCTAssertTrue(command.contains("src/wscrpt tools/previewd/bin/previewctl.mjs"))
        XCTAssertFalse(command.contains("~/src"))
        XCTAssertFalse(command.contains("\n"))
    }

    func testNodeOptionLikeToolsPathRemainsAScriptOperand() throws {
        let builder = try RemotePreviewCommandBuilder(
            workspacePath: "/srv/game",
            previewToolsPath: "--eval=process.exit(99)//"
        )

        let command = builder.list()
        XCTAssertTrue(command.contains("node -- "))
        XCTAssertTrue(
            command.contains("--eval=process.exit(99)//previewd/bin/previewctl.mjs")
        )
    }

    func testDescribeCarriesBoundPortAndPresentation() throws {
        let builder = try RemotePreviewCommandBuilder(workspacePath: "/srv/game")
        let command = try builder.describe(
            sessionID: "session-019d",
            remotePort: 7_331,
            localPort: 49_152,
            profile: .expandedHeadroom,
            presentation: .expanded
        )

        XCTAssertTrue(command.contains("--issue-token"))
        XCTAssertTrue(command.contains("--local-port 49152"))
        XCTAssertTrue(command.contains("--expected-remote-port 7331"))
        XCTAssertTrue(command.contains("--profile expanded-headroom"))
        XCTAssertTrue(command.contains("--presentation expanded"))
    }

    func testCommandBuilderExpandsOnlyLeadingHomeMarker() throws {
        let builder = try RemotePreviewCommandBuilder(
            workspacePath: "~/projects/BIRDWORLD"
        )
        let command = builder.list()

        XCTAssertTrue(command.contains("\"$HOME\"/"))
        XCTAssertTrue(command.contains("projects/BIRDWORLD"))
        XCTAssertFalse(command.contains("cd '~"))
    }

    func testForwardDestinationAndAddressGatesAreNumericLoopbackOnly() throws {
        XCTAssertEqual(
            try SSHForwardDestination(host: "127.0.0.1", port: 7_331),
            try SSHForwardDestination(host: "127.0.0.1", port: 7_331)
        )
        XCTAssertThrowsError(
            try SSHForwardDestination(host: "localhost", port: 7_331)
        )
        XCTAssertThrowsError(
            try SSHForwardDestination(host: "127.0.0.1", port: 0)
        )

        XCTAssertTrue(
            SSHLocalForward.isExactLoopbackListener(
                try SocketAddress(ipAddress: "127.0.0.1", port: 49_152)
            )
        )
        XCTAssertFalse(
            SSHLocalForward.isExactLoopbackListener(
                try SocketAddress(ipAddress: "0.0.0.0", port: 49_152)
            )
        )
    }

    private func listPayload(
        host: String,
        port: Int,
        state: String = "ready",
        healthActive: Bool = true
    ) -> Data {
        Data(
            """
            {
              "protocolVersion": 1,
              "sessions": [
                {
                  "sessionId": "session-019d",
                  "state": "\(state)",
                  "signaling": {"host": "\(host)", "port": \(port), "path": "/signal"},
                  "target": {"sourceWidth": 1280, "sourceHeight": 720},
                  "health": {
                    "heartbeatFresh": true,
                    "tmuxAlive": true,
                    "active": \(healthActive)
                  }
                }
              ]
            }
            """.utf8
        )
    }
}
