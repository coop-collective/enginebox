import { connectGitHub } from "@/lib/github-server";
import { errorResponse } from "@/lib/server";
export async function POST(request: Request) { try { return await connectGitHub(request); } catch (error) { return errorResponse(error); } }
