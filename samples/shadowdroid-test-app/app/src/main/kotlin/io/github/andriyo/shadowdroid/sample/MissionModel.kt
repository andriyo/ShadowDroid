package io.github.andriyo.shadowdroid.sample

import kotlin.math.roundToInt

internal const val MISSION_CODE = "NIGHT-42"

internal data class MissionTransition(
    val stage: Int,
    val status: String,
    val accepted: Boolean,
)

/**
 * Pure contract for Field Lab's guided mission.
 *
 * The Compose screen owns presentation state, while every gate and transition
 * goes through this reducer. That keeps the workflow deterministic for agents
 * and lets JVM tests exhaustively exercise the same rules used on-device.
 */
internal object MissionModel {
    const val BRIEFING = 0
    const val INCIDENT_CLAIMED = 1
    const val RELAY_ARMED = 2
    const val RECOVERY_ACKNOWLEDGED = 3

    fun claimIncident(
        currentStage: Int,
        operatorName: String,
        runCode: String,
    ): MissionTransition =
        when {
            operatorName.isBlank() -> rejected(currentStage, "Operator callsign is required")
            runCode != MISSION_CODE -> rejected(currentStage, "Run code rejected")
            else ->
                accepted(
                    currentStage = currentStage,
                    nextStage = INCIDENT_CLAIMED,
                    status = "Incident claimed by $operatorName",
                )
        }

    fun canArmRelay(
        currentStage: Int,
        signal: Float,
        telemetryEnabled: Boolean,
    ): Boolean =
        currentStage >= INCIDENT_CLAIMED &&
            signal in 68f..74f &&
            telemetryEnabled

    fun armRelay(
        currentStage: Int,
        signal: Float,
        telemetryEnabled: Boolean,
    ): MissionTransition =
        if (canArmRelay(currentStage, signal, telemetryEnabled)) {
            accepted(
                currentStage = currentStage,
                nextStage = RELAY_ARMED,
                status = "Relay calibrated at ${signal.roundToInt()}%",
            )
        } else {
            rejected(currentStage, "Relay calibration requirements are not met")
        }

    fun canAcknowledgeRecovery(
        currentStage: Int,
        selectedRelay: String?,
    ): Boolean = currentStage >= RELAY_ARMED && !selectedRelay.isNullOrBlank()

    fun acknowledgeRecovery(
        currentStage: Int,
        selectedRelay: String?,
    ): MissionTransition =
        if (canAcknowledgeRecovery(currentStage, selectedRelay)) {
            accepted(
                currentStage = currentStage,
                nextStage = RECOVERY_ACKNOWLEDGED,
                status = "Recovery acknowledged for Relay ${selectedRelay.orEmpty()}",
            )
        } else {
            rejected(currentStage, "Select an armed relay before acknowledgement")
        }

    private fun accepted(
        currentStage: Int,
        nextStage: Int,
        status: String,
    ): MissionTransition =
        MissionTransition(
            stage = maxOf(normalizeStage(currentStage), nextStage),
            status = status,
            accepted = true,
        )

    private fun rejected(
        currentStage: Int,
        status: String,
    ): MissionTransition =
        MissionTransition(
            stage = normalizeStage(currentStage),
            status = status,
            accepted = false,
        )

    private fun normalizeStage(stage: Int): Int = stage.coerceIn(BRIEFING, RECOVERY_ACKNOWLEDGED)
}
