package io.github.andriyo.shadowdroid.agent.okhttp

import io.github.andriyo.shadowdroid.agent.Intercept
import okhttp3.MediaType.Companion.toMediaTypeOrNull
import okhttp3.Response
import okhttp3.ResponseBody.Companion.toResponseBody

internal data class MutationResult(
    val response: Response,
    val capturedBody: BodyCapture,
    val responseType: String?,
)

/**
 * Applies an explicit response mutation without conflating a capture preview
 * with the response body. A status-only mutation retains the original body,
 * body object, content headers, and streaming behaviour.
 */
internal fun applyResponseMutation(
    response: Response,
    action: Intercept.Action.Mutate,
    capturedBody: BodyCapture,
    responseType: String?,
): MutationResult {
    val status = action.status ?: response.code
    val replacement = action.body
    if (replacement == null) {
        return MutationResult(
            response = response.newBuilder().code(status).build(),
            capturedBody = capturedBody,
            responseType = responseType,
        )
    }

    val mediaTypeValue = action.contentType ?: responseType ?: "application/json"
    val mediaType = mediaTypeValue.toMediaTypeOrNull()
    val replacementBody = replacement.toResponseBody(mediaType)
    val actualMediaType = replacementBody.contentType()
    val builder = response.newBuilder()
        .code(status)
        // The replacement is plaintext and has a new length. Retaining either
        // framing header would make OkHttp's callers misread it.
        .removeHeader("Content-Encoding")
        .removeHeader("Content-Length")
        .body(replacementBody)
    if (actualMediaType == null) {
        builder.removeHeader("Content-Type")
    } else {
        builder.header("Content-Type", actualMediaType.toString())
    }
    val mutated = builder.build()
    response.body?.close()
    return MutationResult(
        response = mutated,
        capturedBody = captureUtf8(replacement),
        responseType = actualMediaType?.toString(),
    )
}
