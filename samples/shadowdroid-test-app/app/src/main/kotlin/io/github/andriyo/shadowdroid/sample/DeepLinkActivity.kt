package io.github.andriyo.shadowdroid.sample

import android.app.Activity
import android.os.Bundle
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView
import kotlin.math.roundToInt

class DeepLinkActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val root = LinearLayout(this).apply {
            id = R.id.deep_link_root
            orientation = LinearLayout.VERTICAL
            setPadding(18.dp, 18.dp, 18.dp, 18.dp)
        }
        root.addView(TextView(this).apply {
            id = R.id.deep_link_message
            text = "Deep link: ${intent.data ?: "none"}"
            textSize = 20f
            contentDescription = "Deep link activity message"
        })
        root.addView(Button(this).apply {
            id = R.id.deep_link_finish_button
            text = "Finish deep link"
            isAllCaps = false
            contentDescription = "Finish deep link activity button"
            setOnClickListener { finish() }
        })
        setContentView(root)
    }

    private val Int.dp: Int
        get() = (this * resources.displayMetrics.density).roundToInt()
}

