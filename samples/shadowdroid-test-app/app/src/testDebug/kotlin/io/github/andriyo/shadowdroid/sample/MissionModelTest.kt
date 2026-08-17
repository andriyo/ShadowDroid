package io.github.andriyo.shadowdroid.sample

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class MissionModelTest {
    @Test
    fun claimRequiresBothOperatorAndExactCode() {
        assertRejected(
            MissionModel.claimIncident(MissionModel.BRIEFING, "   ", MISSION_CODE),
            MissionModel.BRIEFING,
        )
        assertRejected(
            MissionModel.claimIncident(MissionModel.BRIEFING, "night-agent", "night-42"),
            MissionModel.BRIEFING,
        )

        val claimed = MissionModel.claimIncident(MissionModel.BRIEFING, "night-agent", MISSION_CODE)
        assertTrue(claimed.accepted)
        assertEquals(MissionModel.INCIDENT_CLAIMED, claimed.stage)
    }

    @Test
    fun relayGateIncludesBothSignalBoundariesAndTelemetry() {
        assertTrue(MissionModel.canArmRelay(MissionModel.INCIDENT_CLAIMED, 68f, true))
        assertTrue(MissionModel.canArmRelay(MissionModel.INCIDENT_CLAIMED, 74f, true))
        assertFalse(MissionModel.canArmRelay(MissionModel.INCIDENT_CLAIMED, 67.99f, true))
        assertFalse(MissionModel.canArmRelay(MissionModel.INCIDENT_CLAIMED, 74.01f, true))
        assertFalse(MissionModel.canArmRelay(MissionModel.INCIDENT_CLAIMED, 71f, false))
        assertFalse(MissionModel.canArmRelay(MissionModel.BRIEFING, 71f, true))
        assertEquals(
            "Relay calibrated at 72%",
            MissionModel.armRelay(MissionModel.INCIDENT_CLAIMED, 71.6f, true).status,
        )
    }

    @Test
    fun acknowledgementRequiresAnArmedSelectedRelay() {
        assertFalse(MissionModel.canAcknowledgeRecovery(MissionModel.INCIDENT_CLAIMED, "North"))
        assertFalse(MissionModel.canAcknowledgeRecovery(MissionModel.RELAY_ARMED, null))
        assertFalse(MissionModel.canAcknowledgeRecovery(MissionModel.RELAY_ARMED, ""))

        val acknowledged =
            MissionModel.acknowledgeRecovery(MissionModel.RELAY_ARMED, "North")
        assertTrue(acknowledged.accepted)
        assertEquals(MissionModel.RECOVERY_ACKNOWLEDGED, acknowledged.stage)
    }

    @Test
    fun completeMissionFollowsTheThreeDeclaredGates() {
        val claimed = MissionModel.claimIncident(0, "night-agent", MISSION_CODE)
        val armed = MissionModel.armRelay(claimed.stage, 71f, true)
        val acknowledged = MissionModel.acknowledgeRecovery(armed.stage, "East")

        assertEquals(1, claimed.stage)
        assertEquals(2, armed.stage)
        assertEquals(3, acknowledged.stage)
        assertTrue(listOf(claimed, armed, acknowledged).all(MissionTransition::accepted))
    }

    @Test
    fun acceptedAndRejectedEventsNeverMoveACompletedMissionBackward() {
        for (stage in MissionModel.BRIEFING..MissionModel.RECOVERY_ACKNOWLEDGED) {
            val transitions =
                listOf(
                    MissionModel.claimIncident(stage, "operator", MISSION_CODE),
                    MissionModel.claimIncident(stage, "", "wrong"),
                    MissionModel.armRelay(stage, 71f, true),
                    MissionModel.armRelay(stage, 0f, false),
                    MissionModel.acknowledgeRecovery(stage, "North"),
                    MissionModel.acknowledgeRecovery(stage, null),
                )
            assertTrue(transitions.all { it.stage >= stage })
        }
    }

    @Test
    fun corruptPersistedStagesAreClampedBeforeUse() {
        assertEquals(
            MissionModel.BRIEFING,
            MissionModel.claimIncident(-100, "", "wrong").stage,
        )
        assertEquals(
            MissionModel.RECOVERY_ACKNOWLEDGED,
            MissionModel.claimIncident(100, "", "wrong").stage,
        )
    }

    private fun assertRejected(
        transition: MissionTransition,
        expectedStage: Int,
    ) {
        assertFalse(transition.accepted)
        assertEquals(expectedStage, transition.stage)
    }
}
