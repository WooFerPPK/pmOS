export class WindowBar {
    constructor(element, windowManager) {
        this.element = element;
        this.windowManager = windowManager;
        this.isFullScreen = false;

        this.initBar();
        this.initTitle();
        this.initControls();
        this.addEventListeners();
    }

    initBar() {
        this.bar = this.createElement('div', 'draggable-bar');
        this.element.appendChild(this.bar);
    }

    initTitle() {
        const defaultTitle = 'Untitled';
        this.title = this.createElement('span', 'window-title', this.element.getAttribute('data-title') || defaultTitle);
        this.bar.appendChild(this.title);
    }

    initControls() {
        this.controlsContainer = this.createElement('div', 'controls-container');
        this.bar.appendChild(this.controlsContainer);

        this.closeButton = this.createElement('button', 'close-btn');
        this.controlsContainer.appendChild(this.closeButton);

        this.fullScreenButton = this.createElement('button', 'fullscreen-btn');
        this.controlsContainer.appendChild(this.fullScreenButton);
    }

    createElement(tagName, className, textContent) {
        const element = document.createElement(tagName);
        if (className) element.className = className;
        if (textContent) element.innerText = textContent;
        return element;
    }

    addEventListeners() {
        this.closeButton.addEventListener('click', () => {
            this.windowManager.removeWindow(this.element);
        });

        this.fullScreenButton.addEventListener('click', this.toggleFullScreen.bind(this));
    }

    toggleFullScreen() {
        if (this.isFullScreen) {
            this.exitFullScreen();
        } else {
            this.enterFullScreen();
        }
        this.isFullScreen = !this.isFullScreen;
    }

    enterFullScreen() {
        this.storeOriginalStyles();
        
        this.element.style.width = '100vw';
        this.element.style.height = '100vh';
        this.element.style.top = '0';
        this.element.style.left = '0';

        this.fullScreenButton.className = 'smallscreen-btn';
    }

    exitFullScreen() {
        this.element.style.width = this.originalWidth || '';
        this.element.style.height = this.originalHeight || '';
        this.element.style.top = this.originalTop || '';
        this.element.style.left = this.originalLeft || '';

        this.fullScreenButton.className = 'fullscreen-btn';
    }

    storeOriginalStyles() {
        this.originalWidth = this.element.style.width;
        this.originalHeight = this.element.style.height;
        this.originalTop = this.element.style.top;
        this.originalLeft = this.element.style.left;
    }

    forceExitFullScreen() {
        if (this.isFullScreen) {
            this.exitFullScreen();
            this.isFullScreen = false;
        }
    }
}
