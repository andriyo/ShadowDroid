package io.github.andriyo.shadowdroid.sample

import android.annotation.SuppressLint
import android.view.View
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.WebView
import android.webkit.WebViewClient
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.animateContentSize
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilterChip
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.NavigationBarItemDefaults
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveableStateHolder
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.draw.shadow
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.testTagsAsResourceId
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import kotlin.math.roundToInt

data class LabActions(
    val setStatus: (String) -> Unit,
    val startUnstableUpdates: () -> Unit,
    val showPopup: (View) -> Unit,
    val showToast: () -> Unit,
    val requestCameraPermission: () -> Unit,
    val postNotification: () -> Unit,
    val openDetail: () -> Unit,
    val openDelayedDetail: () -> Unit,
    val openDeepLink: () -> Unit,
    val copyClipboard: () -> Unit,
    val writeSampleFiles: (String, Int) -> Unit,
    val openCoroutines: () -> Unit,
    val emitLogs: () -> Unit,
    val crashNow: () -> Unit,
    val blockMainThread: () -> Unit,
    val openWebSocketChat: () -> Unit,
    val runRequest:
        (
            label: String,
            method: String,
            url: String,
            body: String?,
            headers: Map<String, String>,
        ) -> Unit,
)

private enum class LabDestination(
    val label: String,
    val shortLabel: String,
    val marker: String,
) {
    Overview("Overview", "Home", "01"),
    Mission("Mission", "Mission", "02"),
    Signals("Signals", "Signals", "03"),
    Lab("Lab", "Lab", "04"),
}

@Composable
@OptIn(ExperimentalMaterial3Api::class)
fun ShadowLabApp(
    status: String,
    events: List<String>,
    networkBusy: Boolean,
    counter: Int,
    onIncrementCounter: () -> Unit,
    actions: LabActions,
) {
    var destinationName by rememberSaveable { mutableStateOf(LabDestination.Overview.name) }
    val destination = LabDestination.valueOf(destinationName)
    var showEvents by rememberSaveable { mutableStateOf(false) }
    var operatorName by rememberSaveable { mutableStateOf("agent name") }
    var endpoint by rememberSaveable { mutableStateOf(DEFAULT_HTTPS_URL) }
    var requestBody by rememberSaveable { mutableStateOf(DEFAULT_GRAPHQL_BODY) }
    var missionCode by rememberSaveable { mutableStateOf("") }
    var missionStage by rememberSaveable { mutableIntStateOf(0) }
    val destinationStateHolder = rememberSaveableStateHolder()

    Box(
        modifier =
            Modifier
                .fillMaxSize()
                .background(
                    Brush.verticalGradient(
                        listOf(
                            LabInk,
                            Color(0xFF0A0D18),
                            LabInk,
                        ),
                    ),
                )
                .semantics { testTagsAsResourceId = true }
                .testTag("sample_root"),
    ) {
        Scaffold(
            containerColor = Color.Transparent,
            contentColor = LabText,
            topBar = {
                LabTopBar(
                    destination = destination,
                    status = status,
                    onOpenEvents = { showEvents = true },
                )
            },
            bottomBar = {
                LabNavigationBar(
                    destination = destination,
                    onDestinationChanged = {
                        destinationName = it.name
                        actions.setStatus("${it.label} workspace opened")
                    },
                )
            },
        ) { contentPadding ->
            AnimatedContent(
                targetState = destination,
                modifier =
                    Modifier
                        .fillMaxSize()
                        .padding(contentPadding),
                transitionSpec = {
                    val forward = targetState.ordinal >= initialState.ordinal
                    if (forward) {
                        (slideInHorizontally { it / 5 } + fadeIn()) togetherWith
                            (slideOutHorizontally { -it / 7 } + fadeOut())
                    } else {
                        (slideInHorizontally { -it / 5 } + fadeIn()) togetherWith
                            (slideOutHorizontally { it / 7 } + fadeOut())
                    }
                },
                label = "lab-destination",
            ) { active ->
                destinationStateHolder.SaveableStateProvider(active.name) {
                    when (active) {
                        LabDestination.Overview ->
                            OverviewScreen(
                                missionStage = missionStage,
                                counter = counter,
                                events = events,
                                onOpenMission = { destinationName = LabDestination.Mission.name },
                                onOpenSignals = { destinationName = LabDestination.Signals.name },
                                onOpenLab = { destinationName = LabDestination.Lab.name },
                            )

                        LabDestination.Mission ->
                            MissionScreen(
                                operatorName = operatorName,
                                onOperatorChanged = { operatorName = it },
                                missionCode = missionCode,
                                onMissionCodeChanged = { missionCode = it },
                                missionStage = missionStage,
                                onMissionStageChanged = { missionStage = it },
                                onOpenSignals = { destinationName = LabDestination.Signals.name },
                                actions = actions,
                            )

                        LabDestination.Signals ->
                            SignalsScreen(
                                endpoint = endpoint,
                                onEndpointChanged = { endpoint = it },
                                requestBody = requestBody,
                                onRequestBodyChanged = { requestBody = it },
                                networkBusy = networkBusy,
                                actions = actions,
                            )

                        LabDestination.Lab ->
                            FixtureLabScreen(
                                operatorName = operatorName,
                                counter = counter,
                                onIncrementCounter = onIncrementCounter,
                                actions = actions,
                            )
                    }
                }
            }
        }
    }

    if (showEvents) {
        ModalBottomSheet(
            onDismissRequest = { showEvents = false },
            containerColor = LabPanelRaised,
            contentColor = LabText,
            modifier =
                Modifier
                    .semantics { testTagsAsResourceId = true }
                    .testTag("event_drawer"),
        ) {
            EventDrawer(events = events, onDismiss = { showEvents = false })
        }
    }
}

@Composable
private fun LabTopBar(
    destination: LabDestination,
    status: String,
    onOpenEvents: () -> Unit,
) {
    Surface(
        color = LabDeep.copy(alpha = 0.96f),
        contentColor = LabText,
        tonalElevation = 0.dp,
        shadowElevation = 8.dp,
        modifier = Modifier.statusBarsPadding(),
    ) {
        Column(
            Modifier
                .fillMaxWidth()
                .padding(horizontal = 18.dp, vertical = 10.dp),
        ) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                LabMark()
                Spacer(Modifier.width(11.dp))
                Column(Modifier.weight(1f)) {
                    Text(
                        "SHADOWDROID",
                        style = MaterialTheme.typography.labelSmall,
                        color = LabCyan,
                    )
                    Text(
                        destination.label,
                        style = MaterialTheme.typography.titleLarge,
                    )
                }
                OutlinedButton(
                    onClick = onOpenEvents,
                    contentPadding = PaddingValues(horizontal = 13.dp, vertical = 7.dp),
                    modifier =
                        Modifier
                            .testTag("event_drawer_button")
                            .semantics { contentDescription = "Open recent event drawer" },
                ) {
                    Text("EVENTS", style = MaterialTheme.typography.labelSmall)
                }
            }
            Spacer(Modifier.height(9.dp))
            Row(
                modifier =
                    Modifier
                        .fillMaxWidth()
                        .clip(RoundedCornerShape(12.dp))
                        .background(LabPanel)
                        .testTag("status_text")
                        .semantics { contentDescription = "Current sample status: $status" }
                        .padding(horizontal = 12.dp, vertical = 8.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Box(
                    Modifier
                        .size(8.dp)
                        .background(LabMint, CircleShape),
                )
                Spacer(Modifier.width(9.dp))
                Text(
                    text = status,
                    style = MaterialTheme.typography.bodyMedium,
                    color = LabTextMuted,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
    }
}

@Composable
private fun LabMark() {
    Canvas(
        modifier =
            Modifier
                .size(38.dp)
                .semantics { contentDescription = "ShadowDroid Field Lab mark" },
    ) {
        drawCircle(
            brush = Brush.radialGradient(listOf(LabVioletBright, LabViolet, Color.Transparent)),
            radius = size.minDimension / 2,
            alpha = 0.24f,
        )
        drawCircle(
            color = LabViolet,
            radius = size.minDimension * 0.34f,
            style = Stroke(width = 2.5.dp.toPx()),
        )
        drawLine(
            color = LabCyan,
            start = Offset(size.width * 0.22f, size.height * 0.62f),
            end = Offset(size.width * 0.78f, size.height * 0.38f),
            strokeWidth = 3.dp.toPx(),
            cap = StrokeCap.Round,
        )
        drawCircle(
            color = LabText,
            radius = 3.dp.toPx(),
            center = center,
        )
    }
}

@Composable
private fun LabNavigationBar(
    destination: LabDestination,
    onDestinationChanged: (LabDestination) -> Unit,
) {
    NavigationBar(
        containerColor = LabDeep,
        tonalElevation = 0.dp,
    ) {
        LabDestination.entries.forEach { item ->
            NavigationBarItem(
                selected = destination == item,
                onClick = { onDestinationChanged(item) },
                icon = {
                    Box(
                        modifier =
                            Modifier
                                .size(26.dp)
                                .clip(RoundedCornerShape(8.dp))
                                .background(
                                    if (destination == item) {
                                        LabViolet.copy(alpha = 0.22f)
                                    } else {
                                        Color.Transparent
                                    },
                                ),
                        contentAlignment = Alignment.Center,
                    ) {
                        Text(
                            item.marker,
                            style = MaterialTheme.typography.labelSmall,
                            color = if (destination == item) LabVioletBright else LabTextMuted,
                        )
                    }
                },
                label = { Text(item.shortLabel) },
                colors =
                    NavigationBarItemDefaults.colors(
                        selectedIconColor = LabVioletBright,
                        selectedTextColor = LabText,
                        indicatorColor = Color.Transparent,
                        unselectedIconColor = LabTextMuted,
                        unselectedTextColor = LabTextMuted,
                    ),
                modifier =
                    Modifier
                        .testTag("nav_${item.name.lowercase()}")
                        .semantics { contentDescription = "Open ${item.label} workspace" },
            )
        }
    }
}

@Composable
private fun OverviewScreen(
    missionStage: Int,
    counter: Int,
    events: List<String>,
    onOpenMission: () -> Unit,
    onOpenSignals: () -> Unit,
    onOpenLab: () -> Unit,
) {
    LazyColumn(
        modifier = Modifier.fillMaxSize(),
        contentPadding = PaddingValues(18.dp, 18.dp, 18.dp, 32.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        item {
            OperationHero(
                progress = missionStage / 3f,
                missionStage = missionStage,
                onOpenMission = onOpenMission,
            )
        }
        item {
            SectionHeading(
                eyebrow = "LIVE POSTURE",
                title = "A compact view of a complicated app",
                copy = "The surface is calm; the state model underneath is deliberately demanding.",
            )
        }
        item {
            Row(
                modifier = Modifier.fillMaxWidth(),
                horizontalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                MetricCard(
                    label = "UI NODES",
                    value = "Mixed",
                    accent = LabViolet,
                    modifier = Modifier.weight(1f),
                )
                MetricCard(
                    label = "RETRIES",
                    value = counter.toString().padStart(2, '0'),
                    accent = LabAmber,
                    modifier = Modifier.weight(1f),
                )
                MetricCard(
                    label = "RELAY",
                    value = "Live",
                    accent = LabMint,
                    modifier = Modifier.weight(1f),
                )
            }
        }
        item {
            SectionHeading(
                eyebrow = "WORKSPACES",
                title = "Choose your next surface",
                copy = "Each route has deterministic difficulty and direct fixture access.",
            )
        }
        item {
            LazyRow(
                horizontalArrangement = Arrangement.spacedBy(12.dp),
                contentPadding = PaddingValues(end = 8.dp),
            ) {
                item {
                    WorkspaceCard(
                        code = "02",
                        title = "Recover signal",
                        copy = "Validation, calibration, state gates and a long-press acknowledgement.",
                        accent = LabViolet,
                        testTag = "overview_open_mission",
                        onClick = onOpenMission,
                    )
                }
                item {
                    WorkspaceCard(
                        code = "03",
                        title = "Inspect traffic",
                        copy = "HTTP variants, WSS channel, async state and an embedded WebView.",
                        accent = LabCyan,
                        testTag = "overview_open_signals",
                        onClick = onOpenSignals,
                    )
                }
                item {
                    WorkspaceCard(
                        code = "04",
                        title = "Stress the tool",
                        copy = "Native Views, Compose, windows, permissions, files and fault injection.",
                        accent = LabCoral,
                        testTag = "overview_open_lab",
                        onClick = onOpenLab,
                    )
                }
            }
        }
        item {
            EventPreview(events = events.take(3))
        }
    }
}

@Composable
private fun OperationHero(
    progress: Float,
    missionStage: Int,
    onOpenMission: () -> Unit,
) {
    Card(
        shape = RoundedCornerShape(28.dp),
        colors = CardDefaults.cardColors(containerColor = Color.Transparent, contentColor = LabText),
        modifier =
            Modifier
                .fillMaxWidth()
                .shadow(18.dp, RoundedCornerShape(28.dp), ambientColor = LabViolet.copy(alpha = 0.3f))
                .background(
                    Brush.linearGradient(
                        listOf(
                            Color(0xFF2E235D),
                            Color(0xFF17263E),
                            Color(0xFF12343C),
                        ),
                    ),
                    RoundedCornerShape(28.dp),
                )
                .testTag("overview_mission_card"),
    ) {
        Column(Modifier.padding(22.dp)) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                StatusPill("ACTIVE RUN", LabMint)
                Spacer(Modifier.weight(1f))
                Text(
                    "NIGHT SHIFT · 42",
                    style = MaterialTheme.typography.labelSmall,
                    color = LabText.copy(alpha = 0.72f),
                )
            }
            Spacer(Modifier.height(30.dp))
            Text(
                "Recover the\nsilent relay.",
                style = MaterialTheme.typography.displaySmall,
                color = LabText,
            )
            Spacer(Modifier.height(10.dp))
            Text(
                "A guided incident spanning app state, selectors, permissions, network traffic and recovery.",
                style = MaterialTheme.typography.bodyLarge,
                color = LabText.copy(alpha = 0.76f),
            )
            Spacer(Modifier.height(24.dp))
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(14.dp),
            ) {
                MissionProgress(progress = progress)
                Column(Modifier.weight(1f)) {
                    Text(
                        "${missionStage.coerceAtMost(3)} of 3 gates cleared",
                        style = MaterialTheme.typography.titleMedium,
                    )
                    Text(
                        if (missionStage == 0) "Briefing awaits an operator" else "Run can be resumed safely",
                        style = MaterialTheme.typography.bodyMedium,
                        color = LabText.copy(alpha = 0.66f),
                    )
                }
            }
            Spacer(Modifier.height(22.dp))
            Button(
                onClick = onOpenMission,
                colors =
                    ButtonDefaults.buttonColors(
                        containerColor = LabText,
                        contentColor = LabInk,
                    ),
                modifier =
                    Modifier
                        .fillMaxWidth()
                        .testTag("overview_resume_mission")
                        .semantics { contentDescription = "Resume guided recovery mission" },
            ) {
                Text(if (missionStage == 0) "Start guided run" else "Resume guided run")
            }
        }
    }
}

@Composable
private fun MissionProgress(progress: Float) {
    val animated by animateFloatAsState(progress.coerceIn(0f, 1f), label = "mission-progress")
    Box(Modifier.size(62.dp), contentAlignment = Alignment.Center) {
        Canvas(Modifier.fillMaxSize()) {
            drawArc(
                color = LabText.copy(alpha = 0.16f),
                startAngle = -90f,
                sweepAngle = 360f,
                useCenter = false,
                style = Stroke(6.dp.toPx(), cap = StrokeCap.Round),
            )
            drawArc(
                color = LabMint,
                startAngle = -90f,
                sweepAngle = 360f * animated,
                useCenter = false,
                style = Stroke(6.dp.toPx(), cap = StrokeCap.Round),
            )
        }
        Text(
            "${(animated * 100).roundToInt()}",
            style = MaterialTheme.typography.labelLarge,
        )
    }
}

@Composable
private fun MetricCard(
    label: String,
    value: String,
    accent: Color,
    modifier: Modifier = Modifier,
) {
    Card(
        modifier = modifier,
        shape = RoundedCornerShape(18.dp),
        colors = CardDefaults.cardColors(containerColor = LabPanel, contentColor = LabText),
        border = androidx.compose.foundation.BorderStroke(1.dp, LabOutline.copy(alpha = 0.7f)),
    ) {
        Column(Modifier.padding(14.dp)) {
            Box(
                Modifier
                    .size(8.dp)
                    .background(accent, CircleShape),
            )
            Spacer(Modifier.height(12.dp))
            Text(value, style = MaterialTheme.typography.titleLarge)
            Text(label, style = MaterialTheme.typography.labelSmall, color = LabTextMuted)
        }
    }
}

@Composable
private fun WorkspaceCard(
    code: String,
    title: String,
    copy: String,
    accent: Color,
    testTag: String,
    onClick: () -> Unit,
) {
    Card(
        onClick = onClick,
        modifier =
            Modifier
                .width(268.dp)
                .testTag(testTag)
                .semantics { contentDescription = "Open $title workspace" },
        shape = RoundedCornerShape(22.dp),
        colors = CardDefaults.cardColors(containerColor = LabPanel, contentColor = LabText),
        border = androidx.compose.foundation.BorderStroke(1.dp, accent.copy(alpha = 0.35f)),
    ) {
        Column(Modifier.padding(18.dp)) {
            Text(code, style = MaterialTheme.typography.labelSmall, color = accent)
            Spacer(Modifier.height(24.dp))
            Text(title, style = MaterialTheme.typography.titleLarge)
            Spacer(Modifier.height(7.dp))
            Text(copy, style = MaterialTheme.typography.bodyMedium, color = LabTextMuted)
            Spacer(Modifier.height(18.dp))
            Text("OPEN WORKSPACE  →", style = MaterialTheme.typography.labelSmall, color = accent)
        }
    }
}

@Composable
private fun EventPreview(events: List<String>) {
    LabPanelCard {
        SectionHeading(
            eyebrow = "RECENT ACTIVITY",
            title = "Event trail",
            copy = "Persistent semantic status remains available above every destination.",
        )
        Spacer(Modifier.height(12.dp))
        if (events.isEmpty()) {
            Text("No events recorded yet.", color = LabTextMuted)
        } else {
            events.forEachIndexed { index, event ->
                EventRow(index = index, event = event)
                if (index != events.lastIndex) {
                    HorizontalDivider(
                        color = LabOutline.copy(alpha = 0.65f),
                        modifier = Modifier.padding(vertical = 10.dp),
                    )
                }
            }
        }
    }
}

@Composable
private fun EventRow(index: Int, event: String) {
    Row(verticalAlignment = Alignment.Top) {
        Text(
            (index + 1).toString().padStart(2, '0'),
            style = MaterialTheme.typography.labelSmall,
            color = LabViolet,
        )
        Spacer(Modifier.width(12.dp))
        Text(
            event,
            style = MaterialTheme.typography.bodyMedium,
            color = LabTextMuted,
            modifier = Modifier.weight(1f),
        )
    }
}

@Composable
private fun EventDrawer(
    events: List<String>,
    onDismiss: () -> Unit,
) {
    Column(
        modifier =
            Modifier
                .fillMaxWidth()
                .padding(horizontal = 22.dp)
                .padding(bottom = 30.dp),
    ) {
        Text("Event trail", style = MaterialTheme.typography.headlineSmall)
        Spacer(Modifier.height(6.dp))
        Text(
            "The latest visible app transitions, newest first.",
            style = MaterialTheme.typography.bodyMedium,
            color = LabTextMuted,
        )
        Spacer(Modifier.height(18.dp))
        events.take(8).forEachIndexed { index, event ->
            EventRow(index, event)
            HorizontalDivider(
                color = LabOutline.copy(alpha = 0.65f),
                modifier = Modifier.padding(vertical = 12.dp),
            )
        }
        Button(
            onClick = onDismiss,
            modifier =
                Modifier
                    .fillMaxWidth()
                    .testTag("event_drawer_close"),
        ) {
            Text("Return to workspace")
        }
    }
}

@Composable
@OptIn(ExperimentalMaterial3Api::class)
private fun MissionScreen(
    operatorName: String,
    onOperatorChanged: (String) -> Unit,
    missionCode: String,
    onMissionCodeChanged: (String) -> Unit,
    missionStage: Int,
    onMissionStageChanged: (Int) -> Unit,
    onOpenSignals: () -> Unit,
    actions: LabActions,
) {
    var signal by rememberSaveable { mutableFloatStateOf(42f) }
    var telemetryEnabled by rememberSaveable { mutableStateOf(false) }
    var selectedRelay by rememberSaveable { mutableStateOf<String?>(null) }
    var showCompleteDialog by rememberSaveable { mutableStateOf(false) }
    var codeAttempted by rememberSaveable { mutableStateOf(false) }

    LazyColumn(
        modifier =
            Modifier
                .fillMaxSize()
                .imePadding(),
        contentPadding = PaddingValues(18.dp, 18.dp, 18.dp, 36.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        item {
            SectionHeading(
                eyebrow = "GUIDED SCENARIO",
                title = "Night Shift: Signal Recovery",
                copy = "Three deterministic gates; every state has a visible postcondition.",
            )
        }
        item {
            MissionGateCard(
                number = "01",
                title = "Claim the incident",
                copy = "Use a native EditText for the operator and validate the exact run code.",
                complete = missionStage >= 1,
            ) {
                Text("OPERATOR CALLSIGN", style = MaterialTheme.typography.labelSmall, color = LabTextMuted)
                Spacer(Modifier.height(7.dp))
                PlatformTextField(
                    idValue = R.id.name_input,
                    value = operatorName,
                    label = "Operator callsign",
                    description = "Name input",
                    onValueChanged = onOperatorChanged,
                )
                Spacer(Modifier.height(10.dp))
                OutlinedTextField(
                    value = missionCode,
                    onValueChange = onMissionCodeChanged,
                    singleLine = true,
                    label = { Text("Run code") },
                    supportingText = {
                        if (codeAttempted && missionCode != MISSION_CODE) {
                            Text("Hint: NIGHT-42", color = LabCoral)
                        } else {
                            Text("Use the briefing code")
                        }
                    },
                    isError = codeAttempted && missionCode != MISSION_CODE,
                    keyboardOptions = KeyboardOptions(imeAction = ImeAction.Done),
                    modifier =
                        Modifier
                            .fillMaxWidth()
                            .testTag("mission_code_input")
                            .semantics { contentDescription = "Mission run code input" },
                )
                Spacer(Modifier.height(10.dp))
                Button(
                    onClick = {
                        codeAttempted = true
                        val transition =
                            MissionModel.claimIncident(
                                currentStage = missionStage,
                                operatorName = operatorName,
                                runCode = missionCode,
                            )
                        onMissionStageChanged(transition.stage)
                        actions.setStatus(transition.status)
                    },
                    modifier =
                        Modifier
                            .fillMaxWidth()
                            .testTag("mission_claim_button")
                            .semantics { contentDescription = "Claim signal recovery incident" },
                ) {
                    Text(if (missionStage >= 1) "Incident claimed" else "Validate & claim")
                }
            }
        }
        item {
            MissionGateCard(
                number = "02",
                title = "Tune the relay",
                copy = "Bring signal into the 68–74 window and enable telemetry before arming.",
                complete = missionStage >= 2,
                locked = missionStage < 1,
            ) {
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.SpaceBetween,
                ) {
                    Text("SIGNAL GAIN", style = MaterialTheme.typography.labelSmall, color = LabTextMuted)
                    Text("${signal.roundToInt()}%", style = MaterialTheme.typography.labelSmall, color = LabCyan)
                }
                androidx.compose.material3.Slider(
                    value = signal,
                    onValueChange = { signal = it },
                    enabled = missionStage >= 1,
                    valueRange = 0f..100f,
                    thumb = { InspectorSafeSliderThumb(enabled = missionStage >= 1) },
                    modifier =
                        Modifier
                            .testTag("mission_signal_slider")
                            .semantics { contentDescription = "Mission signal gain slider" },
                )
                Row(
                    modifier =
                        Modifier
                            .fillMaxWidth()
                            .clip(RoundedCornerShape(15.dp))
                            .background(LabDeep)
                            .padding(13.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Column(Modifier.weight(1f)) {
                        Text("Telemetry uplink", style = MaterialTheme.typography.titleMedium)
                        Text(
                            "Required before relay arming",
                            style = MaterialTheme.typography.bodyMedium,
                            color = LabTextMuted,
                        )
                    }
                    Switch(
                        checked = telemetryEnabled,
                        onCheckedChange = { telemetryEnabled = it },
                        enabled = missionStage >= 1,
                        modifier =
                            Modifier
                                .testTag("mission_telemetry_switch")
                                .semantics { contentDescription = "Enable telemetry uplink" },
                    )
                }
                Spacer(Modifier.height(12.dp))
                Button(
                    onClick = {
                        val transition =
                            MissionModel.armRelay(
                                currentStage = missionStage,
                                signal = signal,
                                telemetryEnabled = telemetryEnabled,
                            )
                        onMissionStageChanged(transition.stage)
                        actions.setStatus(transition.status)
                    },
                    enabled = MissionModel.canArmRelay(missionStage, signal, telemetryEnabled),
                    modifier =
                        Modifier
                            .fillMaxWidth()
                            .testTag("mission_arm_relay_button")
                            .semantics { contentDescription = "Arm calibrated relay" },
                ) {
                    Text("Arm calibrated relay")
                }
            }
        }
        item {
            MissionGateCard(
                number = "03",
                title = "Acknowledge recovery",
                copy = "Choose the degraded relay, then long-press the acknowledgement surface.",
                complete = missionStage >= 3,
                locked = missionStage < 2,
            ) {
                Row(
                    modifier =
                        Modifier
                            .fillMaxWidth()
                            .horizontalScroll(rememberScrollState()),
                    horizontalArrangement = Arrangement.spacedBy(10.dp),
                ) {
                    RelayChoice(
                        id = "mission_relay_north",
                        name = "Relay North",
                        detail = "latency 184 ms",
                        selected = selectedRelay == "north",
                        enabled = missionStage >= 2,
                        onClick = {
                            selectedRelay = "north"
                            actions.setStatus("Relay North selected")
                        },
                    )
                    RelayChoice(
                        id = "mission_relay_east",
                        name = "Relay East",
                        detail = "packet loss 12%",
                        selected = selectedRelay == "east",
                        enabled = missionStage >= 2,
                        onClick = {
                            selectedRelay = "east"
                            actions.setStatus("Relay East selected")
                        },
                    )
                }
                Spacer(Modifier.height(14.dp))
                HoldToAcknowledge(
                    enabled = MissionModel.canAcknowledgeRecovery(missionStage, selectedRelay),
                    onClick = { actions.setStatus("Hold acknowledgement; a tap is not enough") },
                    onLongClick = {
                        val transition =
                            MissionModel.acknowledgeRecovery(
                                currentStage = missionStage,
                                selectedRelay = selectedRelay,
                            )
                        onMissionStageChanged(transition.stage)
                        actions.setStatus(transition.status)
                        showCompleteDialog = transition.accepted
                    },
                )
            }
        }
        item {
            LabPanelCard {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Column(Modifier.weight(1f)) {
                        Text("Continue into live traffic", style = MaterialTheme.typography.titleLarge)
                        Text(
                            "The secure channel and HTTP composer live in Signals.",
                            style = MaterialTheme.typography.bodyMedium,
                            color = LabTextMuted,
                        )
                    }
                    TextButton(
                        onClick = onOpenSignals,
                        modifier =
                            Modifier
                                .testTag("mission_open_signals_button")
                                .semantics { contentDescription = "Open Signals workspace from mission" },
                    ) {
                        Text("OPEN →")
                    }
                }
            }
        }
    }

    if (showCompleteDialog) {
        AlertDialog(
            onDismissRequest = { showCompleteDialog = false },
            title = { Text("Signal recovered") },
            text = {
                Text(
                    "The guided state machine is complete. Continue to Signals to generate real network evidence.",
                )
            },
            confirmButton = {
                Button(
                    onClick = {
                        showCompleteDialog = false
                        onOpenSignals()
                    },
                    modifier = Modifier.testTag("mission_complete_open_signals"),
                ) {
                    Text("Open Signals")
                }
            },
            dismissButton = {
                TextButton(
                    onClick = { showCompleteDialog = false },
                    modifier = Modifier.testTag("mission_complete_stay"),
                ) {
                    Text("Stay here")
                }
            },
            modifier =
                Modifier
                    .semantics { testTagsAsResourceId = true }
                    .testTag("mission_complete_dialog"),
        )
    }
}

@Composable
private fun MissionGateCard(
    number: String,
    title: String,
    copy: String,
    complete: Boolean,
    locked: Boolean = false,
    content: @Composable () -> Unit,
) {
    LabPanelCard(
        borderColor =
            when {
                complete -> LabMint.copy(alpha = 0.55f)
                locked -> LabOutline.copy(alpha = 0.45f)
                else -> LabViolet.copy(alpha = 0.45f)
            },
    ) {
        Row(verticalAlignment = Alignment.Top) {
            Box(
                modifier =
                    Modifier
                        .size(38.dp)
                        .background(
                            when {
                                complete -> LabMint.copy(alpha = 0.16f)
                                locked -> LabOutline.copy(alpha = 0.22f)
                                else -> LabViolet.copy(alpha = 0.18f)
                            },
                            RoundedCornerShape(12.dp),
                        ),
                contentAlignment = Alignment.Center,
            ) {
                Text(
                    if (complete) "✓" else number,
                    style = MaterialTheme.typography.labelSmall,
                    color = if (complete) LabMint else if (locked) LabTextMuted else LabVioletBright,
                )
            }
            Spacer(Modifier.width(12.dp))
            Column(Modifier.weight(1f)) {
                Text(title, style = MaterialTheme.typography.titleLarge)
                Spacer(Modifier.height(3.dp))
                Text(copy, style = MaterialTheme.typography.bodyMedium, color = LabTextMuted)
            }
            if (locked) StatusPill("LOCKED", LabTextMuted)
        }
        Spacer(Modifier.height(18.dp))
        Box(Modifier.fillMaxWidth().then(if (locked) Modifier else Modifier)) {
            Column(Modifier.fillMaxWidth(), content = { content() })
        }
    }
}

@Composable
private fun RelayChoice(
    id: String,
    name: String,
    detail: String,
    selected: Boolean,
    enabled: Boolean,
    onClick: () -> Unit,
) {
    FilterChip(
        selected = selected,
        onClick = onClick,
        enabled = enabled,
        label = {
            Column(Modifier.padding(vertical = 5.dp)) {
                Text(name, style = MaterialTheme.typography.labelLarge)
                Text(detail, style = MaterialTheme.typography.labelSmall)
            }
        },
        modifier =
            Modifier
                .widthIn(min = 158.dp)
                .testTag(id)
                .semantics { contentDescription = "Select $name for recovery" },
    )
}

@Composable
@OptIn(ExperimentalFoundationApi::class)
private fun HoldToAcknowledge(
    enabled: Boolean,
    onClick: () -> Unit,
    onLongClick: () -> Unit,
) {
    val color = if (enabled) LabViolet else LabOutline
    Surface(
        color = color.copy(alpha = if (enabled) 0.2f else 0.12f),
        contentColor = LabText,
        shape = RoundedCornerShape(16.dp),
        border = androidx.compose.foundation.BorderStroke(1.dp, color.copy(alpha = 0.65f)),
        modifier =
            Modifier
                .fillMaxWidth()
                .combinedClickable(
                    enabled = enabled,
                    onClick = onClick,
                    onLongClick = onLongClick,
                )
                .testTag("mission_hold_acknowledge")
                .semantics {
                    contentDescription =
                        if (enabled) {
                            "Long press to acknowledge relay recovery"
                        } else {
                            "Recovery acknowledgement locked"
                        }
                },
    ) {
        Row(
            modifier = Modifier.padding(16.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Box(
                Modifier
                    .size(10.dp)
                    .background(color, CircleShape),
            )
            Spacer(Modifier.width(12.dp))
            Column {
                Text("Hold to acknowledge", style = MaterialTheme.typography.titleMedium)
                Text(
                    "A short tap only reports delivery",
                    style = MaterialTheme.typography.bodyMedium,
                    color = LabTextMuted,
                )
            }
        }
    }
}

@Composable
private fun SignalsScreen(
    endpoint: String,
    onEndpointChanged: (String) -> Unit,
    requestBody: String,
    onRequestBodyChanged: (String) -> Unit,
    networkBusy: Boolean,
    actions: LabActions,
) {
    var showAdvanced by rememberSaveable { mutableStateOf(false) }
    var webViewUrl by rememberSaveable { mutableStateOf<String?>(null) }
    var webViewReloadGeneration by rememberSaveable { mutableIntStateOf(0) }

    LazyColumn(
        modifier =
            Modifier
                .fillMaxSize()
                .imePadding(),
        contentPadding = PaddingValues(18.dp, 18.dp, 18.dp, 36.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        item {
            SignalHeader(networkBusy = networkBusy)
        }
        item {
            LabPanelCard {
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Column(Modifier.weight(1f)) {
                        Text("Request composer", style = MaterialTheme.typography.titleLarge)
                        Text(
                            "Native inputs + HttpURLConnection",
                            style = MaterialTheme.typography.bodyMedium,
                            color = LabTextMuted,
                        )
                    }
                    TextButton(
                        onClick = { showAdvanced = !showAdvanced },
                        modifier = Modifier.testTag("network_advanced_toggle"),
                    ) {
                        Text(if (showAdvanced) "COLLAPSE" else "CONFIGURE")
                    }
                }
                Spacer(Modifier.height(14.dp))
                Text("ENDPOINT", style = MaterialTheme.typography.labelSmall, color = LabTextMuted)
                Spacer(Modifier.height(7.dp))
                PlatformTextField(
                    idValue = R.id.url_input,
                    value = endpoint,
                    label = "Network URL",
                    description = "Network URL input",
                    onValueChanged = onEndpointChanged,
                )
                AnimatedVisibility(visible = showAdvanced) {
                    Column {
                        Spacer(Modifier.height(14.dp))
                        Text("JSON BODY", style = MaterialTheme.typography.labelSmall, color = LabTextMuted)
                        Spacer(Modifier.height(7.dp))
                        PlatformTextField(
                            idValue = R.id.body_input,
                            value = requestBody,
                            label = "Request body",
                            description = "Request body input",
                            onValueChanged = onRequestBodyChanged,
                            multiline = true,
                        )
                    }
                }
                Spacer(Modifier.height(15.dp))
                if (networkBusy) {
                    LinearProgressIndicator(
                        modifier =
                            Modifier
                                .fillMaxWidth()
                                .testTag("network_progress")
                                .semantics { contentDescription = "Network request progress" },
                    )
                    Spacer(Modifier.height(12.dp))
                }
                RequestActionGrid(
                    endpoint = endpoint,
                    body = requestBody,
                    enabled = !networkBusy,
                    actions = actions,
                )
            }
        }
        item {
            LabPanelCard(borderColor = LabCyan.copy(alpha = 0.45f)) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Box(
                        Modifier
                            .size(42.dp)
                            .background(LabCyan.copy(alpha = 0.14f), RoundedCornerShape(13.dp)),
                        contentAlignment = Alignment.Center,
                    ) {
                        Text("WS", style = MaterialTheme.typography.labelSmall, color = LabCyan)
                    }
                    Spacer(Modifier.width(13.dp))
                    Column(Modifier.weight(1f)) {
                        Text("Secure field channel", style = MaterialTheme.typography.titleLarge)
                        Text(
                            "OkHttp WS/WSS, binary frames, compression and normal close.",
                            style = MaterialTheme.typography.bodyMedium,
                            color = LabTextMuted,
                        )
                    }
                }
                Spacer(Modifier.height(16.dp))
                Button(
                    onClick = actions.openWebSocketChat,
                    modifier =
                        Modifier
                            .fillMaxWidth()
                            .testTag("websocket_chat_button")
                            .semantics { contentDescription = "Open WebSocket chat button" },
                ) {
                    Text("Open live channel")
                }
            }
        }
        item {
            LabPanelCard {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Column(Modifier.weight(1f)) {
                        Text("Remote diagnostics", style = MaterialTheme.typography.titleLarge)
                        Text(
                            "A platform WebView shares the configured endpoint.",
                            style = MaterialTheme.typography.bodyMedium,
                            color = LabTextMuted,
                        )
                    }
                    StatusPill(if (webViewUrl == null) "IDLE" else "LIVE", if (webViewUrl == null) LabTextMuted else LabMint)
                }
                Spacer(Modifier.height(14.dp))
                OutlinedButton(
                    onClick = {
                        val targetUrl = endpoint.ifBlank { DEFAULT_HTTPS_URL }
                        webViewUrl = targetUrl
                        webViewReloadGeneration += 1
                        actions.setStatus("WebView loading: $targetUrl")
                    },
                    modifier =
                        Modifier
                            .fillMaxWidth()
                            .testTag("webview_button")
                            .semantics { contentDescription = "Load WebView button" },
                ) {
                    Text(if (webViewUrl == null) "Load remote surface" else "Reload remote surface")
                }
                AnimatedVisibility(visible = webViewUrl != null) {
                    Column {
                        Spacer(Modifier.height(14.dp))
                        NetworkWebView(
                            url = webViewUrl.orEmpty(),
                            requestGeneration = webViewReloadGeneration,
                            onStatus = actions.setStatus,
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun SignalHeader(networkBusy: Boolean) {
    Card(
        shape = RoundedCornerShape(24.dp),
        colors = CardDefaults.cardColors(containerColor = LabPanel, contentColor = LabText),
        border = androidx.compose.foundation.BorderStroke(1.dp, LabCyan.copy(alpha = 0.35f)),
    ) {
        Row(
            modifier = Modifier.padding(18.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Box(Modifier.size(58.dp), contentAlignment = Alignment.Center) {
                Canvas(Modifier.fillMaxSize()) {
                    drawCircle(
                        color = LabCyan.copy(alpha = 0.16f),
                        radius = size.minDimension / 2,
                    )
                    drawCircle(
                        color = LabCyan,
                        radius = size.minDimension * 0.27f,
                        style = Stroke(2.dp.toPx()),
                    )
                    drawCircle(
                        color = if (networkBusy) LabAmber else LabMint,
                        radius = 5.dp.toPx(),
                    )
                }
            }
            Spacer(Modifier.width(14.dp))
            Column(Modifier.weight(1f)) {
                Text("Traffic observatory", style = MaterialTheme.typography.headlineSmall)
                Text(
                    if (networkBusy) "A request is in flight" else "Ready for a controlled capture",
                    style = MaterialTheme.typography.bodyMedium,
                    color = LabTextMuted,
                )
            }
        }
    }
}

@Composable
private fun RequestActionGrid(
    endpoint: String,
    body: String,
    enabled: Boolean,
    actions: LabActions,
) {
    val base = endpoint.ifBlank { DEFAULT_HTTPS_URL }
    val actionRows =
        listOf(
            listOf(
                RequestAction("https_get_button", "HTTPS GET", "HTTPS GET button") {
                    actions.runRequest("https-get", "GET", base, null, emptyMap())
                },
                RequestAction("http_get_button", "HTTP GET", "HTTP GET button") {
                    actions.runRequest("http-get", "GET", DEFAULT_HTTP_URL, null, emptyMap())
                },
            ),
            listOf(
                RequestAction("json_post_button", "JSON POST", "JSON POST button") {
                    actions.runRequest("json-post", "POST", base, body, emptyMap())
                },
                RequestAction("graphql_post_button", "GRAPHQL", "GraphQL POST button") {
                    actions.runRequest(
                        "graphql-post",
                        "POST",
                        urlWithPath(base, "/anything/graphql"),
                        body,
                        mapOf("X-GraphQL-Operation" to "ShadowDroidSampleQuery"),
                    )
                },
            ),
            listOf(
                RequestAction("status_418_button", "STATUS 418", "HTTP 418 status button") {
                    actions.runRequest(
                        "status-418",
                        "GET",
                        urlWithPath(base, "/status/418"),
                        null,
                        emptyMap(),
                    )
                },
                RequestAction("slow_request_button", "SLOW 2S", "Slow request button") {
                    actions.runRequest(
                        "slow-request",
                        "GET",
                        urlWithPath(base, "/delay/2"),
                        null,
                        emptyMap(),
                    )
                },
            ),
            listOf(
                RequestAction("large_body_button", "4KB BODY", "Large response button") {
                    actions.runRequest(
                        "large-response",
                        "GET",
                        urlWithPath(base, "/bytes/4096"),
                        null,
                        emptyMap(),
                    )
                },
            ),
        )
    actionRows.forEach { row ->
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(9.dp),
        ) {
            row.forEach { action ->
                OutlinedButton(
                    onClick = action.onClick,
                    enabled = enabled,
                    modifier =
                        Modifier
                            .weight(1f)
                            .testTag(action.id)
                            .semantics { contentDescription = action.description },
                ) {
                    Text(action.label, style = MaterialTheme.typography.labelSmall)
                }
            }
        }
        Spacer(Modifier.height(8.dp))
    }
}

private data class RequestAction(
    val id: String,
    val label: String,
    val description: String,
    val onClick: () -> Unit,
)

private data class WebViewRequest(
    val url: String,
    val generation: Int,
)

@Composable
@SuppressLint("SetJavaScriptEnabled")
private fun NetworkWebView(
    url: String,
    requestGeneration: Int,
    onStatus: (String) -> Unit,
) {
    Box(
        modifier =
            Modifier
                .fillMaxWidth()
                .height(360.dp)
                .clip(RoundedCornerShape(16.dp))
                .background(Color.White)
                .testTag("webview_container")
                .semantics { contentDescription = "WebView container" },
    ) {
        AndroidView(
            modifier = Modifier.fillMaxSize(),
            factory = { context ->
                WebView(context).apply {
                    id = R.id.web_view
                    contentDescription = "Sample WebView"
                    settings.javaScriptEnabled = true
                    webViewClient =
                        object : WebViewClient() {
                            override fun onPageFinished(view: WebView?, loadedUrl: String?) {
                                onStatus("WebView loaded: $loadedUrl")
                            }

                            override fun onReceivedError(
                                view: WebView?,
                                request: WebResourceRequest?,
                                error: WebResourceError?,
                            ) {
                                if (request?.isForMainFrame != false) {
                                    onStatus("WebView error: ${error?.description}")
                                }
                            }
                        }
                    setTag(R.id.web_view, WebViewRequest(url, requestGeneration))
                    loadUrl(url)
                }
            },
            update = { webView ->
                val requested = WebViewRequest(url, requestGeneration)
                if (webView.getTag(R.id.web_view) != requested) {
                    webView.setTag(R.id.web_view, requested)
                    webView.loadUrl(url)
                }
            },
            onRelease = { webView -> webView.destroy() },
        )
    }
}

@Composable
private fun FixtureLabScreen(
    operatorName: String,
    counter: Int,
    onIncrementCounter: () -> Unit,
    actions: LabActions,
) {
    var query by rememberSaveable { mutableStateOf("") }
    var interactionExpanded by rememberSaveable { mutableStateOf(true) }
    var rangeExpanded by rememberSaveable { mutableStateOf(false) }
    var windowsExpanded by rememberSaveable { mutableStateOf(false) }
    var lifecycleExpanded by rememberSaveable { mutableStateOf(false) }
    var faultsExpanded by rememberSaveable { mutableStateOf(false) }
    var showSampleDialog by rememberSaveable { mutableStateOf(false) }
    var pendingFault by rememberSaveable { mutableStateOf<String?>(null) }
    val currentView = LocalView.current

    LazyColumn(
        modifier =
            Modifier
                .fillMaxSize()
                .imePadding()
                .testTag("fixture_lab_scroll"),
        contentPadding = PaddingValues(18.dp, 18.dp, 18.dp, 42.dp),
        verticalArrangement = Arrangement.spacedBy(14.dp),
    ) {
        item {
            SectionHeading(
                eyebrow = "DETERMINISTIC FIXTURES",
                title = "Challenge catalog",
                copy = "Search, expand, scroll, act, and verify. Stable selectors remain mandatory.",
            )
        }
        item {
            OutlinedTextField(
                value = query,
                onValueChange = { query = it },
                label = { Text("Filter labs") },
                singleLine = true,
                keyboardOptions = KeyboardOptions(imeAction = ImeAction.Search),
                modifier =
                    Modifier
                        .fillMaxWidth()
                        .testTag("lab_search_input")
                        .semantics { contentDescription = "Search fixture labs" },
            )
        }

        if (matchesQuery(query, "interactions selectors duplicate nested disabled no-op stress")) {
            item {
                ExpandableLabCard(
                    code = "L1",
                    title = "Interaction gauntlet",
                    copy = "Ambiguity, ancestors, disabled state, delivery-only actions and unstable snapshots.",
                    accent = LabViolet,
                    expanded = interactionExpanded,
                    onExpandedChanged = { interactionExpanded = it },
                    testTag = "lab_interactions_section",
                ) {
                    PlatformSelectorFixtures(
                        counter = counter,
                        onCounterChanged = { onIncrementCounter() },
                        onStatus = actions.setStatus,
                        onStartUnstableUpdates = actions.startUnstableUpdates,
                    )
                }
            }
        }

        if (matchesQuery(query, "calibration range slider compose platform rtl gesture")) {
            item {
                ExpandableLabCard(
                    code = "L2",
                    title = "Calibration matrix",
                    copy = "Native and Compose ranges, disabled and RTL variants, plus a coordinate-only scrubber.",
                    accent = LabCyan,
                    expanded = rangeExpanded,
                    onExpandedChanged = { rangeExpanded = it },
                    testTag = "lab_ranges_section",
                ) {
                    Text(
                        "PLATFORM ACCESSIBILITY",
                        style = MaterialTheme.typography.labelSmall,
                        color = LabCyan,
                    )
                    Spacer(Modifier.height(8.dp))
                    PlatformRangeFixtures(onStatus = actions.setStatus)
                    Spacer(Modifier.height(18.dp))
                    HorizontalDivider(color = LabOutline)
                    Spacer(Modifier.height(18.dp))
                    Text(
                        "COMPOSE SEMANTICS",
                        style = MaterialTheme.typography.labelSmall,
                        color = LabVioletBright,
                    )
                    Spacer(Modifier.height(12.dp))
                    val hostContext = LocalView.current.context
                    AndroidView(
                        factory = {
                            composeSliderFixtures(
                                activity = hostContext,
                                onStatus = actions.setStatus,
                            )
                        },
                        modifier =
                            Modifier
                                .fillMaxWidth()
                                .height(600.dp),
                    )
                }
            }
        }

        if (matchesQuery(query, "windows dialog popup toast permission notification")) {
            item {
                ExpandableLabCard(
                    code = "L3",
                    title = "Windows & permissions",
                    copy = "In-app overlays, watcher surfaces, runtime permissions and notification routing.",
                    accent = LabAmber,
                    expanded = windowsExpanded,
                    onExpandedChanged = { windowsExpanded = it },
                    testTag = "lab_windows_section",
                ) {
                    ActionRows(
                        actions =
                            listOf(
                                LabButtonAction(
                                    "dialog_button",
                                    "Dialog",
                                    "Show alert dialog button",
                                ) { showSampleDialog = true },
                                LabButtonAction(
                                    "popup_button",
                                    "Popup",
                                    "Show popup window button",
                                ) { actions.showPopup(currentView) },
                                LabButtonAction(
                                    "toast_button",
                                    "Toast",
                                    "Show toast button",
                                    actions.showToast,
                                ),
                                LabButtonAction(
                                    "camera_permission_button",
                                    "Camera access",
                                    "Camera permission button",
                                    actions.requestCameraPermission,
                                ),
                                LabButtonAction(
                                    "notification_button",
                                    "Post alert",
                                    "Post notification button",
                                    actions.postNotification,
                                ),
                            ),
                    )
                }
            }
        }

        if (matchesQuery(query, "lifecycle activity deep link clipboard file storage coroutine")) {
            item {
                ExpandableLabCard(
                    code = "L4",
                    title = "Lifecycle & state",
                    copy = "Activities, delayed navigation, deep links, clipboard, private storage and workers.",
                    accent = LabMint,
                    expanded = lifecycleExpanded,
                    onExpandedChanged = { lifecycleExpanded = it },
                    testTag = "lab_lifecycle_section",
                ) {
                    ActionRows(
                        actions =
                            listOf(
                                LabButtonAction(
                                    "detail_button",
                                    "Incident detail",
                                    "Open detail activity button",
                                    actions.openDetail,
                                ),
                                LabButtonAction(
                                    "delayed_detail_button",
                                    "Delayed dispatch",
                                    "Open delayed detail activity button",
                                    actions.openDelayedDetail,
                                ),
                                LabButtonAction(
                                    "deep_link_button",
                                    "Deep-link handoff",
                                    "Open deep link button",
                                    actions.openDeepLink,
                                ),
                                LabButtonAction(
                                    "clipboard_button",
                                    "Copy incident ID",
                                    "Copy clipboard button",
                                    actions.copyClipboard,
                                ),
                                LabButtonAction(
                                    "file_button",
                                    "Export state",
                                    "Write sample files button",
                                ) {
                                    actions.writeSampleFiles(operatorName, counter)
                                },
                                LabButtonAction(
                                    "coroutines_button",
                                    "Telemetry workers",
                                    "Open coroutine workload button",
                                    actions.openCoroutines,
                                ),
                            ),
                    )
                }
            }
        }

        if (matchesQuery(query, "runtime log crash anr fault failure")) {
            item {
                ExpandableLabCard(
                    code = "L5",
                    title = "Runtime & fault injection",
                    copy = "Intentional logs, crash and ANR paths. Destructive actions require confirmation.",
                    accent = LabCoral,
                    expanded = faultsExpanded,
                    onExpandedChanged = { faultsExpanded = it },
                    testTag = "lab_faults_section",
                ) {
                    LabActionButton(
                        id = "log_button",
                        label = "Emit structured log burst",
                        description = "Emit log messages button",
                        onClick = actions.emitLogs,
                    )
                    Spacer(Modifier.height(9.dp))
                    LabActionButton(
                        id = "prepare_crash_button",
                        label = "Prepare deliberate crash",
                        description = "Open crash confirmation",
                        danger = true,
                        onClick = { pendingFault = "crash" },
                    )
                    Spacer(Modifier.height(9.dp))
                    LabActionButton(
                        id = "prepare_anr_button",
                        label = "Prepare 12s main-thread block",
                        description = "Open ANR confirmation",
                        danger = true,
                        onClick = { pendingFault = "anr" },
                    )
                    Spacer(Modifier.height(18.dp))
                    HorizontalDivider(color = LabCoral.copy(alpha = 0.32f))
                    Spacer(Modifier.height(14.dp))
                    Text(
                        "RAW COMPATIBILITY · ONE TAP",
                        style = MaterialTheme.typography.labelSmall,
                        color = LabCoral,
                    )
                    Text(
                        "These legacy IDs execute immediately so existing crash and ANR recipes keep their deterministic contract.",
                        style = MaterialTheme.typography.bodyMedium,
                        color = LabTextMuted,
                    )
                    Spacer(Modifier.height(10.dp))
                    LabActionButton(
                        id = "crash_button",
                        label = "Crash immediately",
                        description = "Crash now button",
                        danger = true,
                        onClick = actions.crashNow,
                    )
                    Spacer(Modifier.height(9.dp))
                    LabActionButton(
                        id = "anr_button",
                        label = "Block main thread immediately",
                        description = "Block main thread button",
                        danger = true,
                        onClick = actions.blockMainThread,
                    )
                }
            }
        }
    }

    if (showSampleDialog) {
        AlertDialog(
            onDismissRequest = {
                showSampleDialog = false
                actions.setStatus("Dialog dismissed")
            },
            title = { Text("Field decision") },
            text = { Text("Choose how the relay incident should proceed. This dialog is safe to watch and automate.") },
            confirmButton = {
                Button(
                    onClick = {
                        showSampleDialog = false
                        actions.setStatus("Dialog accepted")
                    },
                    modifier = Modifier.testTag("dialog_accept_button"),
                ) {
                    Text("Accept")
                }
            },
            dismissButton = {
                Row {
                    TextButton(
                        onClick = {
                            showSampleDialog = false
                            actions.setStatus("Dialog deferred")
                        },
                        modifier = Modifier.testTag("dialog_later_button"),
                    ) {
                        Text("Later")
                    }
                    TextButton(
                        onClick = {
                            showSampleDialog = false
                            actions.setStatus("Dialog cancelled")
                        },
                        modifier = Modifier.testTag("dialog_cancel_button"),
                    ) {
                        Text("Cancel")
                    }
                }
            },
            modifier =
                Modifier
                    .semantics { testTagsAsResourceId = true }
                    .testTag("sample_alert_dialog"),
        )
    }

    pendingFault?.let { fault ->
        AlertDialog(
            onDismissRequest = { pendingFault = null },
            title = { Text(if (fault == "crash") "Crash the sample?" else "Block the main thread?") },
            text = {
                Text(
                    if (fault == "crash") {
                        "The process will terminate immediately so crash detection and recovery can be verified."
                    } else {
                        "The UI will stop responding for 12 seconds so ANR observation can be verified."
                    },
                )
            },
            confirmButton = {
                Button(
                    onClick = {
                        pendingFault = null
                        if (fault == "crash") actions.crashNow() else actions.blockMainThread()
                    },
                    colors = ButtonDefaults.buttonColors(containerColor = LabCoral, contentColor = Color(0xFF3D0010)),
                    modifier = Modifier.testTag(if (fault == "crash") "confirm_crash_button" else "confirm_anr_button"),
                ) {
                    Text(if (fault == "crash") "Crash now" else "Block for 12 seconds")
                }
            },
            dismissButton = {
                TextButton(
                    onClick = { pendingFault = null },
                    modifier = Modifier.testTag("fault_cancel_button"),
                ) {
                    Text("Cancel")
                }
            },
            modifier =
                Modifier
                    .semantics { testTagsAsResourceId = true }
                    .testTag("fault_confirmation_dialog"),
        )
    }
}

@Composable
private fun ExpandableLabCard(
    code: String,
    title: String,
    copy: String,
    accent: Color,
    expanded: Boolean,
    onExpandedChanged: (Boolean) -> Unit,
    testTag: String,
    content: @Composable () -> Unit,
) {
    Card(
        shape = RoundedCornerShape(22.dp),
        colors = CardDefaults.cardColors(containerColor = LabPanel, contentColor = LabText),
        border = androidx.compose.foundation.BorderStroke(1.dp, accent.copy(alpha = 0.32f)),
        modifier =
            Modifier
                .fillMaxWidth()
                .animateContentSize()
                .testTag(testTag),
    ) {
        Column(Modifier.padding(18.dp)) {
            Row(
                modifier = Modifier.fillMaxWidth(),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Box(
                    Modifier
                        .size(40.dp)
                        .background(accent.copy(alpha = 0.14f), RoundedCornerShape(12.dp)),
                    contentAlignment = Alignment.Center,
                ) {
                    Text(code, style = MaterialTheme.typography.labelSmall, color = accent)
                }
                Spacer(Modifier.width(12.dp))
                Column(Modifier.weight(1f)) {
                    Text(title, style = MaterialTheme.typography.titleLarge)
                    Text(copy, style = MaterialTheme.typography.bodyMedium, color = LabTextMuted)
                }
            }
            Spacer(Modifier.height(12.dp))
            OutlinedButton(
                onClick = { onExpandedChanged(!expanded) },
                modifier =
                    Modifier
                        .fillMaxWidth()
                        .testTag("${testTag}_toggle")
                        .semantics {
                            contentDescription =
                                if (expanded) "Collapse $title" else "Expand $title"
                        },
            ) {
                Text(if (expanded) "HIDE FIXTURES" else "OPEN FIXTURES")
            }
            AnimatedVisibility(visible = expanded) {
                Column {
                    Spacer(Modifier.height(16.dp))
                    HorizontalDivider(color = LabOutline)
                    Spacer(Modifier.height(16.dp))
                    content()
                }
            }
        }
    }
}

private data class LabButtonAction(
    val id: String,
    val label: String,
    val description: String,
    val action: () -> Unit,
)

@Composable
private fun ActionRows(actions: List<LabButtonAction>) {
    actions.chunked(2).forEach { rowActions ->
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.spacedBy(9.dp),
        ) {
            rowActions.forEach { action ->
                OutlinedButton(
                    onClick = action.action,
                    modifier =
                        Modifier
                            .weight(1f)
                            .testTag(action.id)
                            .semantics { contentDescription = action.description },
                ) {
                    Text(
                        action.label,
                        style = MaterialTheme.typography.labelLarge,
                        maxLines = 2,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
            }
            if (rowActions.size == 1) Spacer(Modifier.weight(1f))
        }
        Spacer(Modifier.height(9.dp))
    }
}

@Composable
private fun LabActionButton(
    id: String,
    label: String,
    description: String,
    onClick: () -> Unit,
    danger: Boolean = false,
) {
    Button(
        onClick = onClick,
        colors =
            if (danger) {
                ButtonDefaults.buttonColors(
                    containerColor = LabCoral.copy(alpha = 0.18f),
                    contentColor = LabCoral,
                )
            } else {
                ButtonDefaults.buttonColors()
            },
        modifier =
            Modifier
                .fillMaxWidth()
                .testTag(id)
                .semantics { contentDescription = description },
    ) {
        Text(label)
    }
}

@Composable
private fun SectionHeading(
    eyebrow: String,
    title: String,
    copy: String,
) {
    Column {
        Text(eyebrow, style = MaterialTheme.typography.labelSmall, color = LabVioletBright)
        Spacer(Modifier.height(5.dp))
        Text(title, style = MaterialTheme.typography.headlineSmall)
        Spacer(Modifier.height(5.dp))
        Text(copy, style = MaterialTheme.typography.bodyMedium, color = LabTextMuted)
    }
}

@Composable
private fun LabPanelCard(
    borderColor: Color = LabOutline.copy(alpha = 0.7f),
    content: @Composable ColumnScope.() -> Unit,
) {
    Card(
        shape = RoundedCornerShape(22.dp),
        colors = CardDefaults.cardColors(containerColor = LabPanel, contentColor = LabText),
        border = androidx.compose.foundation.BorderStroke(1.dp, borderColor),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(
            modifier = Modifier.padding(18.dp),
            content = content,
        )
    }
}

@Composable
private fun StatusPill(
    label: String,
    color: Color,
) {
    Row(
        modifier =
            Modifier
                .clip(RoundedCornerShape(999.dp))
                .background(color.copy(alpha = 0.14f))
                .border(1.dp, color.copy(alpha = 0.34f), RoundedCornerShape(999.dp))
                .padding(horizontal = 10.dp, vertical = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            Modifier
                .size(6.dp)
                .background(color, CircleShape),
        )
        Spacer(Modifier.width(7.dp))
        Text(label, style = MaterialTheme.typography.labelSmall, color = color)
    }
}

private fun matchesQuery(query: String, haystack: String): Boolean =
    query.isBlank() || haystack.contains(query.trim(), ignoreCase = true)

private fun urlWithPath(baseUrl: String, path: String): String =
    try {
        val base = java.net.URL(baseUrl)
        java.net.URL(base.protocol, base.host, base.port, path).toString()
    } catch (_: Throwable) {
        "https://httpbin.org$path"
    }

const val DEFAULT_HTTPS_URL = "https://httpbin.org/anything/shadowdroid"
const val DEFAULT_HTTP_URL = "http://httpbin.org/anything/shadowdroid-cleartext"
const val DEFAULT_GRAPHQL_BODY =
    """{"operationName":"ShadowDroidSampleQuery","query":"query ShadowDroidSampleQuery { sample: __typename }","variables":{"source":"shadowdroid-test-app"}}"""
