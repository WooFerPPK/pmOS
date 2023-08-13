import StartupManager from '/assets/js/modules/StartupManager/StartupManager.js'

export default class AutoStartupFactory {
    constructor(interactiveWindows) {
        this.startupManager = new StartupManager(interactiveWindows);
    }

    initialize() {
        // Add all the things you want to auto-start here
        this.startupManager.startTerminal();
    }
}