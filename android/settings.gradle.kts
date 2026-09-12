// Plugin versions are pinned here rather than in each module so there is exactly
// one place that decides the AGP/Kotlin pair. This combination (AGP 8.2.0, Kotlin
// 1.9.22, Gradle 8.9) is already in the local Gradle cache, which is the only
// reason it was chosen over something newer.
pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "photosync"
include(":app")
