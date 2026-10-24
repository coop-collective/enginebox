import { githubWebhook } from "@/lib/github-server";
import { errorResponse } from "@/lib/server";
export async function POST(request: Request, context: { params: Promise<{ kind: string }> }) { try { return await githubWebhook(request, (await context.params).kind); } catch (error) { return errorResponse(error); } }
