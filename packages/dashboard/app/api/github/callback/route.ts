import { githubCallback } from "@/lib/github-server";
import { errorResponse } from "@/lib/server";
export async function GET(request: Request) { try { return await githubCallback(request); } catch (error) { return errorResponse(error); } }
