package io.github.andriyo.shadowdroid.sample

import android.app.Activity
import android.os.Bundle
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView
import kotlin.math.roundToInt

/**
 * Launches the [CoroutineWorkload] zoo on open, so simply starting this activity
 * (via a tap or `am start .../CoroutinesActivity`) makes the app's coroutine
 * state worth dumping with `shadowdroid aar coroutines`.
 */
class CoroutinesActivity : Activity() {
    private lateinit var status: TextView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val root = LinearLayout(this).apply {
            id = R.id.coroutines_root
            orientation = LinearLayout.VERTICAL
            setPadding(18.dp, 18.dp, 18.dp, 18.dp)
        }
        root.addView(TextView(this).apply {
            text = "Coroutine workload"
            textSize = 22f
            contentDescription = "Coroutine workload title"
        })
        status = TextView(this).apply {
            id = R.id.coroutines_status
            textSize = 15f
            contentDescription = "Coroutine workload status"
            setPadding(0, 12.dp, 0, 12.dp)
        }
        root.addView(status)
        root.addView(Button(this).apply {
            id = R.id.coroutines_spawn_button
            text = "Spawn another worker"
            isAllCaps = false
            contentDescription = "Spawn coroutine worker button"
            setOnClickListener { status.text = CoroutineWorkload.spawnWorker() }
        })

        status.text = CoroutineWorkload.startOnce()
        setContentView(root)
    }

    private val Int.dp: Int
        get() = (this * resources.displayMetrics.density).roundToInt()
}
