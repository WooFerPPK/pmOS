export class WindowFactory {
    static create(title) {
        const container = this.createWindowContainer(title);
        this.appendToDesktop(container);
        return container;
    }

    static createWindowContainer(title) {
        const container = document.createElement('div');
        container.setAttribute('data-title', title);
        container.classList.add('window');
        return container;
    }

    static appendToDesktop(container) {
        const desktopDiv = document.querySelector('#desktop');
        desktopDiv.appendChild(container);
    }
}