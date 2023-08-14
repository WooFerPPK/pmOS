import StartupManager from '/assets/js/modules/StartupManager/StartupManager.js'

export default class AutoStartupFactory {
    constructor(interactiveWindows, observable) {
        this.startupManager = new StartupManager(interactiveWindows, observable);
    }

    initialize() {
        // Add all the things you want to auto-start here
        this.startupManager.startTerminal();
    }
}