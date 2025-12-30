interface Env {
  // Add your bindings here
}

export default {
  async fetch(request: Request, env: Env, ctx: ExecutionContext): Promise<Response> {
    const url = new URL(request.url);

    // SSE streaming endpoint
    if (url.pathname === "/slow") {
      const target = parseInt(url.searchParams.get("target") || "5") || 5;

      const stream = new ReadableStream({
        async start(controller) {
          for (let i = 1; i <= target; i++) {
            await new Promise((resolve) => setTimeout(resolve, 1000));
            controller.enqueue(`data: ${JSON.stringify({ elapsed: i, target })}\n\n`);
          }

          controller.enqueue(`data: ${JSON.stringify({ done: true, target })}\n\n`);
          controller.close();
        },
      });

      return new Response(stream, {
        headers: {
          "Content-Type": "text/event-stream",
          "Cache-Control": "no-cache",
          Connection: "keep-alive",
        },
      });
    }

    // Test waitUntil - response returns immediately, background work continues
    if (url.pathname === "/waituntil") {
      ctx.waitUntil(
        (async () => {
          console.log("[waitUntil] Starting background work...");
          for (let i = 1; i <= 10; i++) {
            await new Promise((resolve) => setTimeout(resolve, 1000));
            console.log(`[waitUntil] Working... ${i}/10`);
          }
          console.log("[waitUntil] Background work completed!");
        })()
      );

      return new Response("Response sent immediately, background work continues for 10s");
    }

    return new Response("<h1>Hello World</h1>", {
      headers: { "Content-Type": "text/html" },
    });
  },

  async scheduled(event: ScheduledEvent, env: Env, ctx: ExecutionContext): Promise<void> {
    console.log(`Scheduled event at ${new Date(event.scheduledTime).toISOString()}`);
  },
};
