import { path } from "@tauri-apps/api";
import { resourceDir } from "@tauri-apps/api/path";
import { join } from "pathe";
import { fileService } from "@/services/invoke";

let cachedAppDataDir: string | null = null;

interface PathInitResult {
	resourceDir: string;
	appDataDir: string;
}

/**
 * 初始化所有路径缓存
 * 应该在应用启动时调用一次
 */
export const initPathCache = async (): Promise<PathInitResult> => {
	const [resourceDirPath, systemAppDataDir] = await Promise.all([
		resourceDir(),
		path.appDataDir(),
	]);
	const baseResourceDir = join(resourceDirPath, "resources");
	const portableModeResult = await fileService.isPortableMode();
	const appDataDir = systemAppDataDir ?? baseResourceDir;
	const resolvedAppDataDir = portableModeResult.is_portable
		? baseResourceDir
		: appDataDir;
	cachedAppDataDir = resolvedAppDataDir;

	return {
		resourceDir: baseResourceDir,
		appDataDir: resolvedAppDataDir,
	};
};

export const getAppDataDirPath = (): string => {
	if (!cachedAppDataDir) {
		throw new Error(
			"❌ 严重错误：路径缓存未初始化！请确保在访问文件系统前已完成 initPathCache。",
		);
	}
	return cachedAppDataDir;
};
