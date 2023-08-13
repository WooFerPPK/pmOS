import StartupManager from '/assets/js/modules/StartupManager/StartupManager.js'

export class IconFactory {
    constructor(observable, interactiveWindows) {
        this.startupManager = new StartupManager(observable, interactiveWindows);
        this.iconActions = {
            'terminal-icon': this.startupManager.startTerminal.bind(this.startupManager),
            'resume-icon': this.startupManager.startPDFViewer.bind(this.startupManager, ['/assets/templates/pdf/Paul-Moscuzza-Resume.pdf']),
        };
    }

    initialize() {
        Object.entries(this.iconActions).forEach(([iconId, createWindowFunction]) => {
            const iconElement = document.querySelector(`#${iconId}`);
            iconElement.addEventListener('click', createWindowFunction);
        });
    }
}