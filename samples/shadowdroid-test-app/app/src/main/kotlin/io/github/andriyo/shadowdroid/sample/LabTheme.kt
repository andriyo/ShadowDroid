package io.github.andriyo.shadowdroid.sample

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.material3.Typography
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.sp

val LabInk = Color(0xFF080A12)
val LabDeep = Color(0xFF0D101C)
val LabPanel = Color(0xFF141827)
val LabPanelRaised = Color(0xFF1A2032)
val LabViolet = Color(0xFF9B8CFF)
val LabVioletBright = Color(0xFFB8ADFF)
val LabCyan = Color(0xFF55D6E8)
val LabMint = Color(0xFF65E6AF)
val LabAmber = Color(0xFFFFC46B)
val LabCoral = Color(0xFFFF7C8D)
val LabText = Color(0xFFF3F4FF)
val LabTextMuted = Color(0xFFA9AFC3)
val LabOutline = Color(0xFF343B52)

private val LabDarkColors =
    darkColorScheme(
        primary = LabViolet,
        onPrimary = Color(0xFF17102F),
        primaryContainer = Color(0xFF30265B),
        onPrimaryContainer = Color(0xFFE4DEFF),
        secondary = LabCyan,
        onSecondary = Color(0xFF002F36),
        secondaryContainer = Color(0xFF123942),
        onSecondaryContainer = Color(0xFFC4F6FF),
        tertiary = LabMint,
        onTertiary = Color(0xFF003824),
        tertiaryContainer = Color(0xFF154A38),
        onTertiaryContainer = Color(0xFFBFF9D9),
        error = LabCoral,
        onError = Color(0xFF41000B),
        errorContainer = Color(0xFF5B1D2A),
        onErrorContainer = Color(0xFFFFD9DE),
        background = LabInk,
        onBackground = LabText,
        surface = LabDeep,
        onSurface = LabText,
        surfaceVariant = LabPanel,
        onSurfaceVariant = LabTextMuted,
        outline = LabOutline,
        outlineVariant = Color(0xFF252B3D),
        scrim = Color.Black,
    )

@Suppress("unused")
private val LabLightFallback =
    lightColorScheme(
        primary = Color(0xFF51449D),
        secondary = Color(0xFF006879),
        tertiary = Color(0xFF006C4A),
    )

private val LabTypography =
    Typography(
        displaySmall =
            TextStyle(
                fontFamily = FontFamily.SansSerif,
                fontWeight = FontWeight.Black,
                fontSize = 34.sp,
                lineHeight = 38.sp,
                letterSpacing = (-0.8).sp,
            ),
        headlineSmall =
            TextStyle(
                fontFamily = FontFamily.SansSerif,
                fontWeight = FontWeight.Bold,
                fontSize = 24.sp,
                lineHeight = 29.sp,
                letterSpacing = (-0.25).sp,
            ),
        titleLarge =
            TextStyle(
                fontFamily = FontFamily.SansSerif,
                fontWeight = FontWeight.Bold,
                fontSize = 20.sp,
                lineHeight = 25.sp,
            ),
        titleMedium =
            TextStyle(
                fontFamily = FontFamily.SansSerif,
                fontWeight = FontWeight.SemiBold,
                fontSize = 16.sp,
                lineHeight = 21.sp,
            ),
        bodyLarge =
            TextStyle(
                fontFamily = FontFamily.SansSerif,
                fontWeight = FontWeight.Normal,
                fontSize = 16.sp,
                lineHeight = 23.sp,
            ),
        bodyMedium =
            TextStyle(
                fontFamily = FontFamily.SansSerif,
                fontWeight = FontWeight.Normal,
                fontSize = 14.sp,
                lineHeight = 20.sp,
            ),
        labelLarge =
            TextStyle(
                fontFamily = FontFamily.SansSerif,
                fontWeight = FontWeight.Bold,
                fontSize = 14.sp,
                lineHeight = 18.sp,
            ),
        labelMedium =
            TextStyle(
                fontFamily = FontFamily.SansSerif,
                fontWeight = FontWeight.SemiBold,
                fontSize = 12.sp,
                lineHeight = 16.sp,
                letterSpacing = 0.35.sp,
            ),
        labelSmall =
            TextStyle(
                fontFamily = FontFamily.Monospace,
                fontWeight = FontWeight.Medium,
                fontSize = 10.sp,
                lineHeight = 14.sp,
                letterSpacing = 0.7.sp,
            ),
    )

@Composable
fun ShadowLabTheme(content: @Composable () -> Unit) {
    MaterialTheme(
        colorScheme = LabDarkColors,
        typography = LabTypography,
    ) {
        CompositionLocalProvider(
            LocalContentColor provides LabText,
            content = content,
        )
    }
}
