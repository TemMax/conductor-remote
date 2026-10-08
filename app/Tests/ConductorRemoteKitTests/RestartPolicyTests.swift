import Foundation
import Testing
@testable import ConductorRemoteKit

@Test func standardDelaysDoubleUpToThirtySeconds() {
    let delays = (1...7).map { RestartPolicy.standard.delay(afterAttempt: $0) }
    #expect(delays == [.seconds(1), .seconds(2), .seconds(4), .seconds(8), .seconds(16), .seconds(30), .seconds(30)])
}

@Test func delaysStayAtThirtySecondsForLargeAttempts() {
    #expect(RestartPolicy.standard.delay(afterAttempt: 100) == .seconds(30))
    #expect(RestartPolicy.standard.delay(afterAttempt: .max) == .seconds(30))
}

@Test func attemptsBelowOneGetTheFirstDelay() {
    #expect(RestartPolicy.standard.delay(afterAttempt: 0) == .seconds(1))
    #expect(RestartPolicy.standard.delay(afterAttempt: -3) == .seconds(1))
}

@Test func standardPolicyValues() {
    #expect(RestartPolicy.standard.healthyRun == .seconds(60))
    #expect(RestartPolicy.standard.maxQuickFailures == 6)
}
