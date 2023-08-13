export class WindowBar {
    constructor(element) {
        this.element = element;
        this.bar = document.createElement('div');
        this.bar.className = 'draggable-bar';
        this.element.appendChild(this.bar);

        // Fetch the title from data-title attribute and set it in the bar
        this.title = document.createElement('span');
        this.title.className = 'window-title';
        this.title.innerText = this.element.getAttribute('data-title') || 'Untitled';
        this.bar.appendChild(this.title);

        // Controls container
        this.controlsContainer = document.createElement('div');
        this.controlsContainer.className = 'controls-container';
        this.bar.appendChild(this.controlsContainer);

        // Close button
        this.closeButton = document.createElement('button');
        this.closeButton.innerText = 'X';
        this.closeButton.className = 'close-btn';
        this.controlsContainer.appendChild(this.closeButton);

        // Full Screen button
        this.fullScreenButton = document.createElement('button');
        this.fullScreenButton.innerText = '[ ]'; // Symbol for entering full screen
        this.fullScreenButton.className = 'fullscreen-btn';
        this.controlsContainer.appendChild(this.fullScreenButton);

        // Set initial full screen state
        this.isFullScreen = false;

        // Add event listeners
        this.closeButton.addEventListener('click', () => this.element.remove());
        this.fullScreenButton.addEventListener('click', this.toggleFullScreen.bind(this));
    }

    toggleFullScreen() {
        if (this.isFullScreen) {
            // Reset styles to the original state
            this.element.style.width = this.originalWidth || '';
            this.element.style.height = this.originalHeight || '';
            this.element.style.top = this.originalTop || '';
            this.element.style.left = this.originalLeft || '';
            this.fullScreenButton.innerText = '[ ]';
            this.fullScreenButton.className = 'fullscreen-btn';
        } else {
            // Store the current state
            this.originalWidth = this.element.style.width;
            this.originalHeight = this.element.style.height;
            this.originalTop = this.element.style.top;
            this.originalLeft = this.element.style.left;

            // Set styles to make the window full-screen
            this.element.style.width = '100vw';
            this.element.style.height = '100vh';
            this.element.style.top = '0';
            this.element.style.left = '0';
            this.fullScreenButton.innerText = '-';
            this.fullScreenButton.className = 'smallscreen-btn';
        }

        this.isFullScreen = !this.isFullScreen;
    }

    forceExitFullScreen() {
        if (this.isFullScreen) {
            this.isFullScreen = false;
            this.fullScreenButton.innerText = '[ ]';
        }
    }
}
