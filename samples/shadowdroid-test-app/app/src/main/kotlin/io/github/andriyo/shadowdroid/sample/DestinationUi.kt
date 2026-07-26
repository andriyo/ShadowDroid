package io.github.andriyo.shadowdroid.sample

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.testTagsAsResourceId
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp

@Composable
fun LabDestinationScreen(
    rootTag: String,
    eyebrow: String,
    title: String,
    summary: String,
    accent: Color,
    content: @Composable () -> Unit,
) {
    Box(
        modifier =
            Modifier
                .fillMaxSize()
                .background(
                    Brush.verticalGradient(
                        listOf(
                            accent.copy(alpha = 0.13f),
                            LabInk,
                            LabInk,
                        ),
                    ),
                )
                .semantics { testTagsAsResourceId = true }
                .testTag(rootTag),
    ) {
        LazyColumn(
            modifier =
                Modifier
                    .fillMaxSize()
                    .statusBarsPadding()
                    .navigationBarsPadding(),
            contentPadding = PaddingValues(20.dp, 30.dp, 20.dp, 40.dp),
            verticalArrangement = Arrangement.spacedBy(18.dp),
        ) {
            item {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Box(
                        modifier =
                            Modifier
                                .size(44.dp)
                                .background(accent.copy(alpha = 0.16f), RoundedCornerShape(14.dp))
                                .border(1.dp, accent.copy(alpha = 0.42f), RoundedCornerShape(14.dp)),
                        contentAlignment = Alignment.Center,
                    ) {
                        Box(
                            Modifier
                                .size(10.dp)
                                .background(accent, CircleShape),
                        )
                    }
                    Spacer(Modifier.width(13.dp))
                    Column(Modifier.weight(1f)) {
                        Text(
                            "SHADOWDROID FIELD LAB",
                            style = MaterialTheme.typography.labelSmall,
                            color = accent,
                        )
                        Text(eyebrow, style = MaterialTheme.typography.titleMedium)
                    }
                }
            }
            item {
                Text(title, style = MaterialTheme.typography.displaySmall)
                Text(
                    summary,
                    style = MaterialTheme.typography.bodyLarge,
                    color = LabTextMuted,
                    modifier = Modifier.padding(top = 8.dp),
                )
            }
            item {
                Card(
                    shape = RoundedCornerShape(24.dp),
                    colors = CardDefaults.cardColors(containerColor = LabPanel, contentColor = LabText),
                    border = androidx.compose.foundation.BorderStroke(1.dp, accent.copy(alpha = 0.35f)),
                ) {
                    Column(
                        modifier = Modifier.padding(18.dp),
                        verticalArrangement = Arrangement.spacedBy(14.dp),
                    ) {
                        content()
                    }
                }
            }
        }
    }
}

@Composable
fun DestinationValue(
    label: String,
    value: String,
    tag: String? = null,
    description: String? = null,
    accent: Color = LabCyan,
) {
    Column(
        modifier =
            Modifier
                .fillMaxWidth()
                .background(LabDeep, RoundedCornerShape(15.dp))
                .then(if (tag != null) Modifier.testTag(tag) else Modifier)
                .then(
                    if (description != null) {
                        Modifier.semantics { contentDescription = description }
                    } else {
                        Modifier
                    },
                )
                .padding(14.dp),
    ) {
        Text(label, style = MaterialTheme.typography.labelSmall, color = accent)
        Text(
            value,
            style = MaterialTheme.typography.bodyLarge,
            maxLines = 4,
            overflow = TextOverflow.Ellipsis,
        )
    }
}

@Composable
fun DestinationButton(
    label: String,
    tag: String,
    description: String,
    onClick: () -> Unit,
) {
    Button(
        onClick = onClick,
        modifier =
            Modifier
                .fillMaxWidth()
                .testTag(tag)
                .semantics { contentDescription = description },
    ) {
        Text(label)
    }
}
